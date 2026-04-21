//! Merkle tree cache operations.
//!
//! Stores and retrieves Merkle tree data keyed by file path and metadata.

use crate::metadata::db::Database;
use rift_common::crypto::{Blake3Hash, MerkleTree};
use std::path::Path;
use std::time::UNIX_EPOCH;
use tokio_rusqlite::rusqlite;
use tokio_rusqlite::Result as SqliteResult;

/// A cached Merkle tree entry.
#[derive(Debug, Clone)]
pub struct MerkleEntry {
    /// The root hash of the Merkle tree
    pub root: Blake3Hash,
    /// The leaf hashes (one per content chunk)
    pub leaf_hashes: Vec<Blake3Hash>,
}

impl Database {
    /// Get a cached Merkle tree entry for a file.
    ///
    /// Returns `None` if:
    /// - The file has no cached entry
    /// - The cached entry is stale (mtime or size changed)
    pub async fn get_merkle(&self, path: &Path) -> SqliteResult<Option<MerkleEntry>> {
        use std::fs;

        let Ok(meta) = fs::metadata(path) else {
            return Ok(None);
        };

        let mtime_ns = meta
            .modified()
            .map(|t| t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0))
            .unwrap_or(0);

        let file_size = meta.len();
        let path_str = path.to_string_lossy().to_string();

        let result = self
            .call(move |conn: &mut rusqlite::Connection| {
                conn.query_row(
                    "SELECT root_hash, leaf_hashes, mtime_ns, file_size
                 FROM merkle_cache
                 WHERE file_path = ?1",
                    [&path_str],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
            })
            .await;

        match result {
            Ok((root, leaf_hashes, cached_mtime, cached_size)) => {
                if cached_mtime == mtime_ns as i64 && cached_size == file_size as i64 {
                    let root = match Blake3Hash::from_slice(&root) {
                        Ok(h) => h,
                        Err(_) => return Ok(None),
                    };
                    let leaf_hashes = match MerkleTree::default().deserialize_leaves(&leaf_hashes) {
                        Ok(h) => h,
                        Err(_) => return Ok(None),
                    };
                    Ok(Some(MerkleEntry { root, leaf_hashes }))
                } else {
                    let path_str = path.to_string_lossy().to_string();
                    let _ = self
                        .call(move |conn: &mut rusqlite::Connection| {
                            conn.execute(
                                "DELETE FROM merkle_cache WHERE file_path = ?1",
                                [&path_str],
                            )
                        })
                        .await;
                    Ok(None)
                }
            }
            Err(e) if e.to_string().contains("QueryReturnedNoRows") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Store a Merkle tree entry for a file.
    ///
    /// Overwrites any existing entry for this file.
    ///
    /// `path` must be a canonical path (i.e. the result of `canonicalize()`).
    /// The handler always canonicalises paths before calling this function.
    /// Tests that pre-populate the cache must do the same, or cache lookups
    /// will silently miss on systems where temp directories involve symlinks
    /// (e.g. macOS `/var` → `/private/var`).
    ///
    /// Takes explicit mtime_ns and file_size rather than reading from the
    /// filesystem, so callers can pass verified/cached values.
    pub async fn put_merkle(
        &self,
        path: &Path,
        mtime_ns: u64,
        file_size: u64,
        root: &Blake3Hash,
        leaf_hashes: &[Blake3Hash],
    ) -> SqliteResult<()> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let merkle = MerkleTree::default();
        let serialized_leaves = merkle.serialize_leaves(leaf_hashes);

        let path_str = path.to_string_lossy().to_string();
        let root_bytes = root.as_bytes().to_vec();
        let serialized = serialized_leaves;

        self.call(move |conn: &mut rusqlite::Connection| {
            conn.execute(
                "INSERT OR REPLACE INTO merkle_cache
                 (file_path, mtime_ns, file_size, root_hash, leaf_hashes, computed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    path_str,
                    mtime_ns as i64,
                    file_size as i64,
                    root_bytes,
                    serialized,
                    now,
                ),
            )
        })
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn file_mtime_ns(path: &Path) -> u64 {
        let meta = fs::metadata(path).unwrap();
        meta.modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    fn file_size(path: &Path) -> u64 {
        fs::metadata(path).unwrap().len()
    }

    #[tokio::test]
    async fn merkle_cache_stores_and_retrieves() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        fs::write(&file_path, b"hello").unwrap();

        let root = Blake3Hash::new(b"test");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &root,
            &leaf_hashes,
        )
        .await
        .unwrap();

        let result = db.get_merkle(&file_path).await.unwrap();
        assert!(result.is_some());

        let entry = result.unwrap();
        assert_eq!(entry.root, root);
        assert_eq!(entry.leaf_hashes, leaf_hashes);
    }

    #[tokio::test]
    async fn merkle_cache_stale_on_mtime_change() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        fs::write(&file_path, b"hello").unwrap();

        let root = Blake3Hash::new(b"test");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &root,
            &leaf_hashes,
        )
        .await
        .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&file_path, b"modified").unwrap();

        let result = db.get_merkle(&file_path).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn merkle_cache_stale_on_size_change() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        fs::write(&file_path, b"hello").unwrap();

        let root = Blake3Hash::new(b"test");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &root,
            &leaf_hashes,
        )
        .await
        .unwrap();

        fs::write(&file_path, b"much longer content").unwrap();

        let result = db.get_merkle(&file_path).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn merkle_cache_persists_across_reopen() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let file_path = temp_dir.path().join("test.txt");

        fs::write(&file_path, b"hello").unwrap();

        let root = Blake3Hash::new(b"test");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        {
            let db = Database::open(&db_path).await.unwrap();
            db.put_merkle(
                &file_path,
                file_mtime_ns(&file_path),
                file_size(&file_path),
                &root,
                &leaf_hashes,
            )
            .await
            .unwrap();
        }

        {
            let db = Database::open(&db_path).await.unwrap();
            let result = db.get_merkle(&file_path).await.unwrap();
            assert!(result.is_some());

            let entry = result.unwrap();
            assert_eq!(entry.root, root);
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn get_merkle_handles_pre_epoch_mtime() {
        // Pre-epoch mtime caused panic on unwrap()
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("pre_epoch.txt");
        fs::write(&file_path, b"test content").unwrap();

        // Set mtime before Unix epoch (1969-12-31)
        let path_c = CString::new(file_path.as_os_str().as_bytes()).unwrap();
        let times = libc::timespec {
            tv_sec: -86400, // one day before epoch
            tv_nsec: 0,
        };
        let times_arr = [times, times]; // atime, mtime
        let ret = unsafe { libc::utimensat(libc::AT_FDCWD, path_c.as_ptr(), times_arr.as_ptr(), 0) };
        assert_eq!(ret, 0, "utimensat failed");

        // Should not panic - function must complete without panic
        let db = Database::open_in_memory().await.unwrap();
        let _result = db.get_merkle(&file_path).await; // If panic occurs, test fails here
    }
}
