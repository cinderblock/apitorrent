//! Library folders and the files their walks found (migration 12). See `crate::folders`.

use rusqlite::{OptionalExtension, params};

use super::Database;

/// A library folder as stored, with totals over its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRow {
    pub id: i64,
    pub path: String,
    pub added_at: i64,
    pub last_scan_at: Option<i64>,
    pub error: Option<String>,
    pub files: u64,
    pub bytes: u64,
    pub hashed_files: u64,
    pub hashed_bytes: u64,
}

/// One file a folder's walk found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderFile {
    pub folder_id: i64,
    /// Absolute path.
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    /// Its BEP 52 `pieces root` and full SHA-256, once fingerprinted at this size and time.
    pub pieces_root: Option<[u8; 32]>,
    pub sha256: Option<[u8; 32]>,
}

fn size_param(size: u64) -> i64 {
    i64::try_from(size).unwrap_or(i64::MAX)
}

fn hash_of(v: Option<Vec<u8>>) -> Option<[u8; 32]> {
    v.and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok())
}

fn file_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FolderFile> {
    Ok(FolderFile {
        folder_id: r.get(0)?,
        path: r.get(1)?,
        size: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
        mtime_ns: r.get(3)?,
        pieces_root: hash_of(r.get(4)?),
        sha256: hash_of(r.get(5)?),
    })
}

const FILE_COLUMNS: &str = "folder_id, path, size, mtime_ns, pieces_root, sha256";

impl Database {
    /// Every library folder, oldest first, with totals over its files.
    pub fn library_folders(&self) -> rusqlite::Result<Vec<FolderRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, f.path, f.added_at, f.last_scan_at, f.error,
                    COUNT(l.path), COALESCE(SUM(l.size), 0),
                    COUNT(l.sha256), COALESCE(SUM(CASE WHEN l.sha256 IS NULL THEN 0 ELSE l.size END), 0)
             FROM library_folders f LEFT JOIN library_files l ON l.folder_id = f.id
             GROUP BY f.id ORDER BY f.id",
        )?;
        stmt.query_map([], |r| {
            let n = |i: usize| -> rusqlite::Result<u64> {
                Ok(u64::try_from(r.get::<_, i64>(i)?).unwrap_or(0))
            };
            Ok(FolderRow {
                id: r.get(0)?,
                path: r.get(1)?,
                added_at: r.get(2)?,
                last_scan_at: r.get(3)?,
                error: r.get(4)?,
                files: n(5)?,
                bytes: n(6)?,
                hashed_files: n(7)?,
                hashed_bytes: n(8)?,
            })
        })?
        .collect()
    }

    /// Add a folder. `Ok(None)` when it is already there.
    pub fn add_library_folder(&self, path: &str, added_at: i64) -> rusqlite::Result<Option<i64>> {
        let n = self.conn.execute(
            "INSERT INTO library_folders (path, added_at) VALUES (?1, ?2)
             ON CONFLICT(path) DO NOTHING",
            params![path, added_at],
        )?;
        Ok((n == 1).then(|| self.conn.last_insert_rowid()))
    }

    /// Forget a folder and its files (the files themselves are never touched). Whether it
    /// was there.
    pub fn remove_library_folder(&self, id: i64) -> rusqlite::Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM library_folders WHERE id = ?1", [id])?
            == 1)
    }

    /// Record the end of a folder's walk, and why it failed if it did.
    pub fn set_folder_scanned(
        &self,
        id: i64,
        at: i64,
        error: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE library_folders SET last_scan_at = ?2, error = ?3 WHERE id = ?1",
            params![id, at, error],
        )?;
        Ok(())
    }

    /// The files a folder's last walk found: path, size and time.
    pub fn folder_stamps(&self, folder_id: i64) -> rusqlite::Result<Vec<(String, u64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, size, mtime_ns FROM library_files WHERE folder_id = ?1")?;
        stmt.query_map([folder_id], |r| {
            Ok((
                r.get(0)?,
                u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                r.get(2)?,
            ))
        })?
        .collect()
    }

    /// Apply what a walk found, in one transaction: new and changed files (a changed file's
    /// fingerprint is cleared), and files gone.
    pub fn apply_folder_scan(
        &mut self,
        folder_id: i64,
        upserts: &[(String, u64, i64)],
        deletes: &[String],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut up = tx.prepare(
                "INSERT INTO library_files (folder_id, path, size, mtime_ns) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(folder_id, path) DO UPDATE SET
                     pieces_root = CASE WHEN size = excluded.size AND mtime_ns = excluded.mtime_ns
                                   THEN pieces_root ELSE NULL END,
                     sha256 = CASE WHEN size = excluded.size AND mtime_ns = excluded.mtime_ns
                              THEN sha256 ELSE NULL END,
                     size = excluded.size, mtime_ns = excluded.mtime_ns",
            )?;
            for (path, size, mtime) in upserts {
                up.execute(params![folder_id, path, size_param(*size), mtime])?;
            }
            let mut del =
                tx.prepare("DELETE FROM library_files WHERE folder_id = ?1 AND path = ?2")?;
            for path in deletes {
                del.execute(params![folder_id, path])?;
            }
        }
        tx.commit()
    }

    /// Record one file, with whatever fingerprint is known (a removed torrent's file kept
    /// under a folder, say). A fingerprint is only stored with the size and time it is for.
    pub fn upsert_folder_file(&self, f: &FolderFile) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO library_files (folder_id, path, size, mtime_ns, pieces_root, sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(folder_id, path) DO UPDATE SET size = excluded.size,
                 mtime_ns = excluded.mtime_ns, pieces_root = excluded.pieces_root,
                 sha256 = excluded.sha256",
            params![
                f.folder_id,
                f.path,
                size_param(f.size),
                f.mtime_ns,
                f.pieces_root.as_ref().map(<[u8; 32]>::as_slice),
                f.sha256.as_ref().map(<[u8; 32]>::as_slice),
            ],
        )?;
        Ok(())
    }

    /// Files not fingerprinted yet, smallest folder id and path first.
    pub fn unhashed_folder_files(&self, limit: usize) -> rusqlite::Result<Vec<FolderFile>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM library_files WHERE sha256 IS NULL
             ORDER BY folder_id, path LIMIT ?1"
        ))?;
        stmt.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], file_from_row)?
            .collect()
    }

    /// Store a file's fingerprint, with the size and time it was computed at. Nothing
    /// happens if the folder forgot the file meanwhile.
    pub fn set_folder_fingerprint(
        &self,
        folder_id: i64,
        path: &str,
        fp: &crate::fingerprint::Fingerprint,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE library_files SET size = ?3, mtime_ns = ?4, pieces_root = ?5, sha256 = ?6
             WHERE folder_id = ?1 AND path = ?2",
            params![
                folder_id,
                path,
                size_param(fp.size),
                fp.mtime_ns,
                fp.pieces_root.as_bytes().as_slice(),
                fp.sha256.as_slice(),
            ],
        )?;
        Ok(())
    }

    /// Bytes of folder files not fingerprinted yet.
    pub fn unhashed_folder_bytes(&self) -> rusqlite::Result<u64> {
        let n: Option<i64> = self
            .conn
            .query_row(
                "SELECT SUM(size) FROM library_files WHERE sha256 IS NULL",
                [],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(n.and_then(|n| u64::try_from(n).ok()).unwrap_or(0))
    }

    /// The folder files of these sizes: the candidates for a new torrent's files.
    pub fn folder_files_by_size(&self, sizes: &[u64]) -> rusqlite::Result<Vec<FolderFile>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM library_files WHERE size = ?1 ORDER BY folder_id, path"
        ))?;
        let mut out = Vec::new();
        for &size in sizes {
            for f in stmt.query_map([size_param(size)], file_from_row)? {
                out.push(f?);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::Fingerprint;

    #[test]
    fn folders_and_their_files() {
        let mut db = Database::open_in_memory().unwrap();
        let id = db.add_library_folder("/media", 5).unwrap().unwrap();
        assert_eq!(db.add_library_folder("/media", 6).unwrap(), None);
        db.apply_folder_scan(
            id,
            &[("/media/a".into(), 10, 1), ("/media/b".into(), 20, 2)],
            &[],
        )
        .unwrap();
        let fp = Fingerprint {
            size: 10,
            mtime_ns: 1,
            pieces_root: apitorrent_metainfo::Sha256Hash([1; 32]),
            sha256: [2; 32],
        };
        db.set_folder_fingerprint(id, "/media/a", &fp).unwrap();
        let rows = db.library_folders().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].files, rows[0].bytes), (2, 30));
        assert_eq!((rows[0].hashed_files, rows[0].hashed_bytes), (1, 10));
        assert_eq!(db.unhashed_folder_bytes().unwrap(), 20);

        // An unchanged rescan keeps the fingerprint; a changed time clears it.
        db.apply_folder_scan(id, &[("/media/a".into(), 10, 1)], &[])
            .unwrap();
        assert_eq!(
            db.folder_files_by_size(&[10]).unwrap()[0].sha256,
            Some([2; 32])
        );
        db.apply_folder_scan(id, &[("/media/a".into(), 10, 3)], &["/media/b".into()])
            .unwrap();
        let a = &db.folder_files_by_size(&[10]).unwrap()[0];
        assert_eq!((a.mtime_ns, a.sha256, a.pieces_root), (3, None, None));
        assert!(db.folder_files_by_size(&[20]).unwrap().is_empty());
        assert_eq!(db.unhashed_folder_files(10).unwrap().len(), 1);

        assert!(db.remove_library_folder(id).unwrap());
        assert!(
            db.folder_files_by_size(&[10]).unwrap().is_empty(),
            "cascade"
        );
        assert!(!db.remove_library_folder(id).unwrap());
    }
}
