//! BEP 52 merkle trees.
//!
//! Each file in a v2 torrent has its own binary SHA-256 tree. The leaves are the hashes of
//! 16 KiB blocks (the last block may be short). The leaf count is padded to a power of two
//! with all-zero leaves. A file's `pieces root` is the root of that tree, and its piece layer
//! is the tree level at which each node covers exactly `piece length` bytes.

use crate::hash::Sha256Hash;

/// The size of a merkle leaf block, fixed by BEP 52.
pub const BLOCK_SIZE: u64 = 16 * 1024;

/// The hash of a subtree whose leaves are all padding. `level` 0 is a single zero leaf, and
/// level `n` covers `2^n` padding leaves.
pub fn pad_hash(level: u32) -> Sha256Hash {
    let mut h = Sha256Hash::ZERO;
    for _ in 0..level {
        h = Sha256Hash::pair(&h, &h);
    }
    h
}

/// The root of a tree whose bottom row is `nodes`, padded with `pad` up to `width`, which
/// must be a power of two not smaller than `nodes.len()`.
///
/// # Panics
///
/// Panics if `width` is not a power of two or is smaller than the number of nodes. Both are
/// programming errors.
pub fn root_from_layer(nodes: &[Sha256Hash], width: usize, pad: Sha256Hash) -> Sha256Hash {
    assert!(width.is_power_of_two() && width >= nodes.len());
    if nodes.is_empty() {
        let mut h = pad;
        let mut w = width;
        while w > 1 {
            h = Sha256Hash::pair(&h, &h);
            w /= 2;
        }
        return h;
    }
    let mut row: Vec<Sha256Hash> = nodes.to_vec();
    let mut pad = pad;
    let mut w = width;
    while w > 1 {
        let next_len = row.len().div_ceil(2);
        let mut next = Vec::with_capacity(next_len);
        for pair in row.chunks(2) {
            let right = pair.get(1).unwrap_or(&pad);
            next.push(Sha256Hash::pair(&pair[0], right));
        }
        pad = Sha256Hash::pair(&pad, &pad);
        row = next;
        w /= 2;
    }
    row[0]
}

/// The number of 16 KiB leaf blocks in a file of `length` bytes.
pub fn block_count(length: u64) -> u64 {
    length.div_ceil(BLOCK_SIZE)
}

/// The root of a file's tree, computed from its piece layer.
///
/// `layer` holds one hash per piece. `blocks_per_piece` is `piece_length / BLOCK_SIZE`.
pub fn root_from_piece_layer(layer: &[Sha256Hash], length: u64, piece_length: u64) -> Sha256Hash {
    let blocks_per_piece = piece_length / BLOCK_SIZE;
    let leaves = block_count(length).next_power_of_two();
    let width = usize::try_from(leaves / blocks_per_piece)
        .unwrap_or(usize::MAX)
        .max(1);
    root_from_layer(layer, width, pad_hash(blocks_per_piece.trailing_zeros()))
}

/// Hashes data incrementally into a file's leaf row, and produces its root and piece layer.
/// Used when creating torrents and when verifying downloaded data.
#[derive(Debug, Default, Clone)]
pub struct FileHasher {
    leaves: Vec<Sha256Hash>,
    length: u64,
    partial: Vec<u8>,
}

impl FileHasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.length += data.len() as u64;
        let block = usize::try_from(BLOCK_SIZE).expect("16 KiB fits in usize");
        if !self.partial.is_empty() {
            let take = (block - self.partial.len()).min(data.len());
            self.partial.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.partial.len() == block {
                self.leaves.push(Sha256Hash::digest(&self.partial));
                self.partial.clear();
            }
        }
        while data.len() >= block {
            self.leaves.push(Sha256Hash::digest(&data[..block]));
            data = &data[block..];
        }
        self.partial.extend_from_slice(data);
    }

    /// The file's `pieces root` and its piece layer. The layer is empty when the file fits in
    /// one piece, because BEP 52 omits piece layers for such files.
    pub fn finish(mut self, piece_length: u64) -> (Sha256Hash, Vec<Sha256Hash>) {
        if !self.partial.is_empty() {
            self.leaves.push(Sha256Hash::digest(&self.partial));
        }
        if self.length == 0 {
            return (Sha256Hash::ZERO, Vec::new());
        }
        let width = self.leaves.len().next_power_of_two();
        let root = root_from_layer(&self.leaves, width, Sha256Hash::ZERO);
        if self.length <= piece_length {
            return (root, Vec::new());
        }
        let per_piece = usize::try_from(piece_length / BLOCK_SIZE).expect("piece fits in usize");
        let pad = Sha256Hash::ZERO;
        let layer = self
            .leaves
            .chunks(per_piece)
            .map(|chunk| root_from_layer(chunk, per_piece, pad))
            .collect();
        (root, layer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piece_layer_root_matches_leaf_root() {
        // 5 pieces of 64 KiB (4 blocks each) plus a short tail: the piece-layer route and
        // the leaf route must agree.
        let piece_length = 4 * BLOCK_SIZE;
        let data: Vec<u8> = (0..(5 * piece_length + 1000))
            .map(|i| (i % 251) as u8)
            .collect();
        let mut hasher = FileHasher::new();
        hasher.update(&data[..12345]);
        hasher.update(&data[12345..]);
        let (root, layer) = hasher.finish(piece_length);
        assert_eq!(layer.len(), 6);
        assert_eq!(
            root_from_piece_layer(&layer, data.len() as u64, piece_length),
            root
        );
    }

    #[test]
    fn small_file_has_no_layer() {
        let mut hasher = FileHasher::new();
        hasher.update(&[7; 100]);
        let (root, layer) = hasher.finish(BLOCK_SIZE * 4);
        assert_eq!(layer, []);
        // A single block: the root is the leaf itself.
        assert_eq!(root, Sha256Hash::digest(&[7; 100]));
    }
}
