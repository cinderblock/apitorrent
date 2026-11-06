//! Piece verification for v1 (SHA-1 over the piece) and v2 (merkle subtrees per file).

use std::collections::HashMap;
use std::sync::Arc;

use apitorrent_metainfo::merkle::{self, BLOCK_SIZE};
use apitorrent_metainfo::{Info, Sha1Hash, Sha256Hash, Torrent};

/// The outcome of checking one piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Valid,
    Invalid,
    /// A v2 piece whose layer we don't have yet. It can't be trusted until the layer
    /// arrives from a peer.
    Unknown,
}

/// What a v2 piece is checked against.
#[derive(Debug, Clone, Copy)]
struct V2Piece {
    /// The file the piece belongs to (pieces never span files in v2).
    file: usize,
    /// The file's first piece.
    first_piece: u32,
}

/// Everything needed to check pieces of one torrent.
#[derive(Debug, Clone)]
pub struct PieceHashes {
    piece_length: u64,
    v1: Arc<[Sha1Hash]>,
    /// For each piece, the v2 file it belongs to, or `None` for pieces covered only by pad
    /// data (they never are; kept for robustness) and for v1-only torrents.
    v2_pieces: Vec<Option<V2Piece>>,
    files: Vec<(u64, u64, Option<Sha256Hash>)>,
    layers: HashMap<Sha256Hash, Arc<[Sha256Hash]>>,
}

impl PieceHashes {
    pub fn new(torrent: &Torrent) -> Self {
        Self::from_info(&torrent.info, torrent.piece_layers.clone())
    }

    pub fn from_info(info: &Info, layers: HashMap<Sha256Hash, Arc<[Sha256Hash]>>) -> Self {
        let piece_length = info.piece_length;
        let count = usize::try_from(info.piece_count()).expect("fits");
        let mut v2_pieces = vec![None; count];
        let files: Vec<(u64, u64, Option<Sha256Hash>)> = info
            .files
            .iter()
            .map(|f| (f.offset, f.length, f.pieces_root))
            .collect();
        if info.has_v2() {
            for (i, f) in info.files.iter().enumerate() {
                if f.is_pad() || f.length == 0 || f.pieces_root.is_none() {
                    continue;
                }
                let first = u32::try_from(f.offset / piece_length).expect("fits");
                let last = u32::try_from((f.offset + f.length - 1) / piece_length).expect("fits");
                for p in first..=last {
                    v2_pieces[p as usize] = Some(V2Piece {
                        file: i,
                        first_piece: first,
                    });
                }
            }
        }
        Self {
            piece_length,
            v1: info.v1_pieces.clone(),
            v2_pieces,
            files,
            layers,
        }
    }

    /// Add a piece layer received from a peer. The caller must have validated it with
    /// [`Info::validate_piece_layer`].
    pub fn add_layer(&mut self, root: Sha256Hash, layer: Arc<[Sha256Hash]>) {
        self.layers.insert(root, layer);
    }

    /// Check `data`, which must be the whole of piece `piece`.
    pub fn verify(&self, piece: u32, data: &[u8]) -> Verdict {
        let idx = piece as usize;
        if let Some(expected) = self.v1.get(idx)
            && Sha1Hash::digest(data) != *expected
        {
            return Verdict::Invalid;
        }
        let Some(v2) = self.v2_pieces.get(idx).copied().flatten() else {
            return if self.v1.is_empty() {
                Verdict::Unknown
            } else {
                Verdict::Valid
            };
        };
        let (offset, length, root) = self.files[v2.file];
        let root = root.expect("v2 pieces map only to files with roots");
        let piece_start = u64::from(piece) * self.piece_length;
        // Only the file's own bytes are hashed; trailing pad bytes belong to no tree.
        let file_bytes = (offset + length - piece_start).min(data.len() as u64);
        let file_data = &data[..usize::try_from(file_bytes).expect("fits")];
        let leaves: Vec<Sha256Hash> = file_data
            .chunks(usize::try_from(BLOCK_SIZE).expect("fits"))
            .map(Sha256Hash::digest)
            .collect();

        let (computed, expected) = if length <= self.piece_length {
            let width = leaves.len().next_power_of_two();
            (
                merkle::root_from_layer(&leaves, width, Sha256Hash::ZERO),
                root,
            )
        } else {
            let Some(layer) = self.layers.get(&root) else {
                // In a hybrid torrent the v1 hash already passed, which is enough to trust
                // the piece. The v2 check happens once the layer arrives.
                return if self.v1.is_empty() {
                    Verdict::Unknown
                } else {
                    Verdict::Valid
                };
            };
            let per_piece = usize::try_from(self.piece_length / BLOCK_SIZE).expect("fits");
            let Some(expected) = layer.get((piece - v2.first_piece) as usize) else {
                return Verdict::Invalid;
            };
            (
                merkle::root_from_layer(&leaves, per_piece, Sha256Hash::ZERO),
                *expected,
            )
        };
        if computed == expected {
            Verdict::Valid
        } else {
            Verdict::Invalid
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};

    fn data(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from(i % 251).unwrap() ^ seed)
            .collect()
    }

    /// Lay out the piece space for files a (70 KB) and b (10 KB) with 32 KiB pieces.
    fn space(version: Version) -> (Torrent, Vec<u8>) {
        let a = data(70_000, 1);
        let b = data(10_000, 2);
        let bytes = TorrentBuilder::new("t", version)
            .piece_length(32 * 1024)
            .file(FileSource::from_bytes(&["a"], a.clone()))
            .file(FileSource::from_bytes(&["b"], b.clone()))
            .build()
            .unwrap();
        let t = Torrent::from_bytes(&bytes).unwrap();
        let mut space = vec![0u8; usize::try_from(t.info.total_length()).unwrap()];
        for f in &t.info.files {
            let start = usize::try_from(f.offset).unwrap();
            if f.path.last().unwrap() == "a" {
                space[start..start + a.len()].copy_from_slice(&a);
            } else if f.path.last().unwrap() == "b" {
                space[start..start + b.len()].copy_from_slice(&b);
            }
        }
        (t, space)
    }

    #[test]
    fn every_version_verifies_good_pieces_and_rejects_bad_ones() {
        for version in [Version::V1, Version::V2, Version::Hybrid] {
            let (t, space) = space(version);
            let hashes = PieceHashes::new(&t);
            let pl = usize::try_from(t.info.piece_length).unwrap();
            for piece in 0..u32::try_from(t.info.piece_count()).unwrap() {
                let start = piece as usize * pl;
                let end = (start + pl).min(space.len());
                let mut chunk = space[start..end].to_vec();
                assert_eq!(
                    hashes.verify(piece, &chunk),
                    Verdict::Valid,
                    "{version:?} {piece}"
                );
                chunk[0] ^= 0xff;
                assert_eq!(
                    hashes.verify(piece, &chunk),
                    Verdict::Invalid,
                    "{version:?} {piece}"
                );
            }
        }
    }

    #[test]
    fn v2_without_layers_is_unknown() {
        let (t, space) = space(Version::V2);
        let hashes = PieceHashes::from_info(&t.info, HashMap::new());
        // File a spans several pieces, so it needs its layer.
        assert_eq!(hashes.verify(0, &space[..32 * 1024]), Verdict::Unknown);
        // File b fits in one piece, so its root is enough.
        let last = u32::try_from(t.info.piece_count()).unwrap() - 1;
        let start = last as usize * 32 * 1024;
        assert_eq!(hashes.verify(last, &space[start..]), Verdict::Valid);
    }
}
