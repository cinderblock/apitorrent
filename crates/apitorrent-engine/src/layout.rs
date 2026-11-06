//! Mapping between the torrent's piece space and its files.

use std::ops::Range;

use apitorrent_metainfo::Info;

/// The standard request size (BEP 3). Also the BEP 52 merkle leaf size.
pub const BLOCK_LEN: u32 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpan {
    pub offset: u64,
    pub length: u64,
    pub pad: bool,
}

/// A contiguous run of bytes within one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub file: usize,
    pub file_offset: u64,
    /// Offset of this segment within the piece-space range that was asked for.
    pub buf_offset: usize,
    pub len: usize,
    pub pad: bool,
}

/// The shape of a torrent's piece space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    piece_length: u64,
    total_length: u64,
    piece_count: u32,
    files: Vec<FileSpan>,
}

impl Layout {
    pub fn from_info(info: &Info) -> Self {
        Self {
            piece_length: info.piece_length,
            total_length: info.total_length(),
            piece_count: u32::try_from(info.piece_count()).expect("piece count fits u32"),
            files: info
                .files
                .iter()
                .map(|f| FileSpan {
                    offset: f.offset,
                    length: f.length,
                    pad: f.is_pad(),
                })
                .collect(),
        }
    }

    pub fn piece_length(&self) -> u64 {
        self.piece_length
    }

    pub fn piece_count(&self) -> u32 {
        self.piece_count
    }

    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    pub fn files(&self) -> &[FileSpan] {
        &self.files
    }

    pub fn piece_size(&self, piece: u32) -> u32 {
        let start = u64::from(piece) * self.piece_length;
        let size = self.piece_length.min(self.total_length - start);
        u32::try_from(size).expect("piece size fits u32")
    }

    pub fn piece_offset(&self, piece: u32) -> u64 {
        u64::from(piece) * self.piece_length
    }

    pub fn blocks_in_piece(&self, piece: u32) -> u32 {
        self.piece_size(piece).div_ceil(BLOCK_LEN)
    }

    pub fn block_size(&self, piece: u32, block: u32) -> u32 {
        let size = self.piece_size(piece);
        (size - block * BLOCK_LEN).min(BLOCK_LEN)
    }

    /// The pieces that hold any byte of file `index`.
    pub fn file_pieces(&self, index: usize) -> Range<u32> {
        let f = &self.files[index];
        if f.length == 0 {
            return 0..0;
        }
        let first = f.offset / self.piece_length;
        let last = (f.offset + f.length - 1) / self.piece_length;
        let to_u32 = |v: u64| u32::try_from(v).expect("piece index fits u32");
        to_u32(first)..to_u32(last) + 1
    }

    /// The file segments covering `len` bytes at `offset` in piece space. Zero-length files
    /// are skipped.
    pub fn segments(&self, offset: u64, len: usize) -> Vec<Segment> {
        let end = offset + len as u64;
        // The first file that ends after `offset`.
        let mut i = self
            .files
            .partition_point(|f| f.offset + f.length <= offset);
        let mut out = Vec::new();
        while i < self.files.len() {
            let f = &self.files[i];
            if f.offset >= end {
                break;
            }
            if f.length > 0 {
                let start = offset.max(f.offset);
                let stop = end.min(f.offset + f.length);
                out.push(Segment {
                    file: i,
                    file_offset: start - f.offset,
                    buf_offset: usize::try_from(start - offset).expect("fits"),
                    len: usize::try_from(stop - start).expect("fits"),
                    pad: f.pad,
                });
            }
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(files: &[(u64, bool)], piece_length: u64) -> Layout {
        let mut offset = 0;
        let files: Vec<FileSpan> = files
            .iter()
            .map(|&(length, pad)| {
                let f = FileSpan {
                    offset,
                    length,
                    pad,
                };
                offset += length;
                f
            })
            .collect();
        Layout {
            piece_length,
            total_length: offset,
            piece_count: u32::try_from(offset.div_ceil(piece_length)).unwrap(),
            files,
        }
    }

    #[test]
    fn segments_span_files() {
        let l = layout(&[(10, false), (0, false), (5, false), (20, false)], 16);
        assert_eq!(l.piece_count(), 3);
        assert_eq!(l.piece_size(2), 3);
        let segs = l.segments(8, 16);
        let summary: Vec<(usize, u64, usize, usize)> = segs
            .iter()
            .map(|s| (s.file, s.file_offset, s.buf_offset, s.len))
            .collect();
        assert_eq!(summary, [(0, 8, 0, 2), (2, 0, 2, 5), (3, 0, 7, 9)]);
    }

    #[test]
    fn file_pieces() {
        let l = layout(&[(10, false), (0, false), (40, false)], 16);
        assert_eq!(l.file_pieces(0), 0..1);
        assert_eq!(l.file_pieces(1), 0..0);
        assert_eq!(l.file_pieces(2), 0..4);
    }

    #[test]
    fn blocks() {
        let l = layout(
            &[(u64::from(BLOCK_LEN) * 2 + 100, false)],
            u64::from(BLOCK_LEN) * 4,
        );
        assert_eq!(l.blocks_in_piece(0), 3);
        assert_eq!(l.block_size(0, 2), 100);
    }
}
