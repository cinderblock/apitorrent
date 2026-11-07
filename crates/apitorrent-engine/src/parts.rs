//! The part store: bytes of unwanted files that arrive as part of a piece the torrent
//! still needs.
//!
//! A piece at a file boundary can hold the end of a wanted file and the start of an
//! unwanted one. The whole piece has to be downloaded and verified, but the unwanted
//! file's bytes must not appear in the save directory. They go here instead, so the piece
//! stays verifiable and seedable.
//!
//! # Format
//!
//! One file per torrent:
//!
//! ```text
//! header   magic "APTPARTS", version u32, piece_length u64, piece_count u32 (little endian)
//! table    piece_count × u32: slot index + 1, or 0 when the piece has no slot
//! (padding to a 4 KiB boundary)
//! slots    piece_length bytes each, holding a piece's bytes at their in-piece offsets
//! ```
//!
//! Only the ranges that belong to unwanted files are written into a slot; the rest of it
//! stays sparse or zero. A slot's data is written and flushed before its table entry, so a
//! crash can only lose a slot, never leave a table entry pointing at garbage. Resume
//! validation then notices the missing slot and drops the piece.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::storage::{long_path, read_exact_at, write_all_at};

const MAGIC: &[u8; 8] = b"APTPARTS";
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 8 + 4 + 8 + 4;
const HEADER_LEN: u64 = HEADER_SIZE as u64;
const ALIGN: u64 = 4096;

#[derive(Debug)]
struct State {
    file: Option<Arc<File>>,
    /// Slot index + 1 for each piece, 0 for none.
    table: Vec<u32>,
    free: BTreeSet<u32>,
    slot_count: u32,
}

/// One torrent's part store. Safe to use from several disk threads at once: slot I/O runs
/// without the lock, which only guards the table.
#[derive(Debug)]
pub struct PartFile {
    path: PathBuf,
    piece_length: u64,
    piece_count: u32,
    state: Mutex<State>,
}

impl PartFile {
    /// Open the part store at `path`, or prepare an empty one. The file is only created on
    /// the first write. A file with a different shape (another torrent's, or a damaged
    /// one) is treated as empty and replaced on the first write.
    pub fn open(path: &Path, piece_length: u64, piece_count: u32) -> Self {
        let path = long_path(path);
        let mut state = State {
            file: None,
            table: vec![0; piece_count as usize],
            free: BTreeSet::new(),
            slot_count: 0,
        };
        match Self::load(&path, piece_length, piece_count) {
            Ok(Some((file, table))) => {
                let used: BTreeSet<u32> =
                    table.iter().filter(|&&s| s != 0).map(|s| s - 1).collect();
                let slot_count = used.last().map_or(0, |&s| s + 1);
                state.free = (0..slot_count).filter(|s| !used.contains(s)).collect();
                state.slot_count = slot_count;
                state.table = table;
                state.file = Some(Arc::new(file));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "ignoring unreadable part store");
            }
        }
        Self {
            path,
            piece_length,
            piece_count,
            state: Mutex::new(state),
        }
    }

    fn load(
        path: &Path,
        piece_length: u64,
        piece_count: u32,
    ) -> io::Result<Option<(File, Vec<u32>)>> {
        let file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut header = [0u8; HEADER_SIZE];
        if read_exact_at(&file, &mut header, 0).is_err() {
            return Ok(None);
        }
        let version = u32::from_le_bytes(header[8..12].try_into().expect("4 bytes"));
        let len = u64::from_le_bytes(header[12..20].try_into().expect("8 bytes"));
        let count = u32::from_le_bytes(header[20..24].try_into().expect("4 bytes"));
        if &header[..8] != MAGIC
            || version != VERSION
            || len != piece_length
            || count != piece_count
        {
            return Ok(None);
        }
        let mut raw = vec![0u8; piece_count as usize * 4];
        read_exact_at(&file, &mut raw, HEADER_LEN)?;
        let table = raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        Ok(Some((file, table)))
    }

    fn slots_start(&self) -> u64 {
        (HEADER_LEN + u64::from(self.piece_count) * 4).next_multiple_of(ALIGN)
    }

    fn slot_offset(&self, slot: u32) -> u64 {
        self.slots_start() + u64::from(slot) * self.piece_length
    }

    /// Whether the piece has a slot.
    pub fn contains(&self, piece: u32) -> bool {
        self.state.lock().expect("parts lock poisoned").table[piece as usize] != 0
    }

    /// The pieces that have a slot.
    pub fn pieces(&self) -> Vec<u32> {
        let state = self.state.lock().expect("parts lock poisoned");
        (0..self.piece_count)
            .filter(|&p| state.table[p as usize] != 0)
            .collect()
    }

    /// Read `buf.len()` bytes at `offset` within `piece`.
    pub fn read(&self, piece: u32, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let (file, slot) = {
            let state = self.state.lock().expect("parts lock poisoned");
            let slot = state.table[piece as usize];
            match (&state.file, slot) {
                (Some(f), s) if s != 0 => (f.clone(), s - 1),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("piece {piece} is not in the part store"),
                    ));
                }
            }
        };
        read_exact_at(&file, buf, self.slot_offset(slot) + offset)
    }

    /// Write `data` at `offset` within `piece`, giving the piece a slot if it has none.
    pub fn write(&self, piece: u32, offset: u64, data: &[u8]) -> io::Result<()> {
        debug_assert!(offset + data.len() as u64 <= self.piece_length);
        let (file, slot, new) = {
            let mut state = self.state.lock().expect("parts lock poisoned");
            let file = self.file(&mut state)?;
            match state.table[piece as usize] {
                0 => {
                    let slot = if let Some(s) = state.free.pop_first() {
                        s
                    } else {
                        state.slot_count += 1;
                        state.slot_count - 1
                    };
                    // Reserved in memory at once, so a concurrent write to the same piece
                    // uses the same slot. On disk the entry comes after the data.
                    state.table[piece as usize] = slot + 1;
                    (file, slot, true)
                }
                s => (file, s - 1, false),
            }
        };
        let result = write_all_at(&file, data, self.slot_offset(slot) + offset).and_then(|()| {
            if new {
                file.sync_data()?;
                write_all_at(
                    &file,
                    &(slot + 1).to_le_bytes(),
                    HEADER_LEN + u64::from(piece) * 4,
                )?;
            }
            Ok(())
        });
        if new && result.is_err() {
            let mut state = self.state.lock().expect("parts lock poisoned");
            state.table[piece as usize] = 0;
            state.free.insert(slot);
        }
        result
    }

    /// Drop a piece's slot. The space is reused by later writes.
    pub fn remove(&self, piece: u32) -> io::Result<()> {
        let mut state = self.state.lock().expect("parts lock poisoned");
        let slot = state.table[piece as usize];
        if slot == 0 {
            return Ok(());
        }
        let file = state.file.clone().expect("a slot implies a file");
        write_all_at(
            &file,
            &0u32.to_le_bytes(),
            HEADER_LEN + u64::from(piece) * 4,
        )?;
        state.table[piece as usize] = 0;
        state.free.insert(slot - 1);
        // Shrink when the last slots are free, and delete the file once it's empty.
        while state.slot_count > 0 {
            let last = state.slot_count - 1;
            if !state.free.remove(&last) {
                break;
            }
            state.slot_count = last;
        }
        if state.slot_count == 0 {
            state.file = None;
            state.free.clear();
            drop(file);
            std::fs::remove_file(&self.path).or_else(ignore_missing)?;
        } else {
            file.set_len(self.slot_offset(state.slot_count))?;
        }
        Ok(())
    }

    /// Delete the store and everything in it.
    pub fn delete(&self) -> io::Result<()> {
        let mut state = self.state.lock().expect("parts lock poisoned");
        state.file = None;
        state.table.fill(0);
        state.free.clear();
        state.slot_count = 0;
        std::fs::remove_file(&self.path).or_else(ignore_missing)
    }

    pub fn sync(&self) -> io::Result<()> {
        let file = self.state.lock().expect("parts lock poisoned").file.clone();
        match file {
            Some(f) => f.sync_data(),
            None => Ok(()),
        }
    }

    fn file(&self, state: &mut State) -> io::Result<Arc<File>> {
        if let Some(f) = &state.file {
            return Ok(f.clone());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.path)?;
        let mut header = Vec::with_capacity(HEADER_SIZE);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&self.piece_length.to_le_bytes());
        header.extend_from_slice(&self.piece_count.to_le_bytes());
        write_all_at(&file, &header, 0)?;
        file.set_len(self.slots_start())?;
        let file = Arc::new(file);
        state.file = Some(file.clone());
        state.table.fill(0);
        state.free.clear();
        state.slot_count = 0;
        Ok(file)
    }
}

fn ignore_missing(e: io::Error) -> io::Result<()> {
    if e.kind() == io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_reopen_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.parts");
        let p = PartFile::open(&path, 1024, 10);
        assert!(!path.exists(), "created lazily");
        p.write(7, 100, &[7; 50]).unwrap();
        p.write(2, 0, &[2; 1024]).unwrap();
        p.write(7, 900, &[8; 124]).unwrap();
        assert_eq!(p.pieces(), [2, 7]);
        p.sync().unwrap();
        drop(p);

        let p = PartFile::open(&path, 1024, 10);
        let mut buf = [0u8; 50];
        p.read(7, 100, &mut buf).unwrap();
        assert_eq!(buf, [7; 50]);
        let mut buf = [0u8; 124];
        p.read(7, 900, &mut buf).unwrap();
        assert_eq!(buf, [8; 124]);
        assert!(p.read(3, 0, &mut buf).is_err());

        // Piece 2 took slot 1 (7 wrote first); freeing 7 leaves slot 0 for reuse.
        p.remove(7).unwrap();
        assert!(!p.contains(7));
        p.write(4, 0, &[4; 10]).unwrap();
        let mut buf = [0u8; 1024];
        p.read(2, 0, &mut buf).unwrap();
        assert_eq!(buf, [2; 1024], "other slots are untouched");

        p.remove(2).unwrap();
        p.remove(4).unwrap();
        assert!(!path.exists(), "an empty store is deleted");
    }

    #[test]
    fn a_store_of_another_shape_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.parts");
        let p = PartFile::open(&path, 1024, 10);
        p.write(1, 0, &[1; 10]).unwrap();
        drop(p);
        let p = PartFile::open(&path, 2048, 10);
        assert!(p.pieces().is_empty());
        std::fs::write(&path, b"garbage").unwrap();
        let p = PartFile::open(&path, 1024, 10);
        assert!(p.pieces().is_empty());
        p.write(3, 0, &[3; 4]).unwrap();
        let mut buf = [0u8; 4];
        p.read(3, 0, &mut buf).unwrap();
        assert_eq!(buf, [3; 4]);
    }
}
