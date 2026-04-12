//! Merkle tree cache operations.
//!
//! Stores and retrieves Merkle tree data keyed by file path and metadata.

use crate::metadata::db::Database;
use rift_common::crypto::Blake3Hash;
use rusqlite::{params, Result as SqliteResult};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub fn get_merkle(&self, path: &Path) -> SqliteResult<Option<MerkleEntry>> {
        use std::fs;

        let conn = self.connection();

        // Get current file metadata
        let Ok(meta) = fs::metadata(path) else {
            return Ok(None); // File doesn't exist
        };

        let Ok(mtime_ns) = meta
            .modified()
            .map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64)
        else {
            return Ok(None);
        };

        let file_size = meta.len();

        // Query the cache
        let result = conn.query_row(
            "SELECT root_hash, leaf_hashes, mtime_ns, file_size
             FROM merkle_cache
             WHERE file_path = ?1",
            params![path.to_string_lossy()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                ))
            },
        );

        match result {
            Ok((root, leaf_hashes, cached_mtime, cached_size)) => {
                // Check staleness
                if cached_mtime == mtime_ns && cached_size == file_size {
                    let root = match Blake3Hash::from_slice(&root) {
                        Ok(h) => h,
                        Err(_) => return Ok(None),
                    };
                    let merkle = rift_common::crypto::MerkleTree::default();
                    let leaf_hashes = match merkle.deserialize_leaves(&leaf_hashes) {
                        Ok(h) => h,
                        Err(_) => return Ok(None),
                    };
                    Ok(Some(MerkleEntry { root, leaf_hashes }))
                } else {
                    // Stale - delete and return None
                    let _ = conn.execute(
                        "DELETE FROM merkle_cache WHERE file_path = ?1",
                        params![path.to_string_lossy()],
                    );
                    Ok(None)
                }
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Store a Merkle tree entry for a file.
    ///
    /// Overwrites any existing entry for this file.
    ///
    /// Takes explicit mtime_ns and file_size rather than reading from the
    /// filesystem, so callers can pass verified/cached values.
    pub fn put_merkle(
        &self,
        path: &Path,
        mtime_ns: u64,
        file_size: u64,
        root: &Blake3Hash,
        leaf_hashes: &[Blake3Hash],
    ) -> SqliteResult<()> {
        let conn = self.connection();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let merkle = rift_common::crypto::MerkleTree::default();
        let serialized_leaves = merkle.serialize_leaves(leaf_hashes);

        conn.execute(
            "INSERT OR REPLACE INTO merkle_cache
             (file_path, mtime_ns, file_size, root_hash, leaf_hashes, computed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                path.to_string_lossy(),
                mtime_ns as i64,
                file_size as i64,
                root.as_bytes(),
                &serialized_leaves,
                now,
            ],
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::UNIX_EPOCH;

    fn file_mtime_ns(path: &Path) -> u64 {
        let meta = fs::metadata(path).unwrap();
        meta.modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    fn file_size(path: &Path) -> u64 {
        fs::metadata(path).unwrap().len()
    }

    #[test]
    fn merkle_cache_miss_on_first_access() {
        let db = Database::open_in_memory().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        fs::write(&file_path, b"hello").unwrap();

        // First access should be a miss
        let result = db.get_merkle(&file_path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn merkle_cache_store_and_retrieve() {
        let db = Database::open_in_memory().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        fs::write(&file_path, b"hello").unwrap();

        // Store a merkle entry
        let root = Blake3Hash::new(b"test");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1"), Blake3Hash::new(b"chunk2")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &root,
            &leaf_hashes,
        )
        .unwrap();

        // Retrieve it
        let result = db.get_merkle(&file_path).unwrap();
        assert!(result.is_some());

        let entry = result.unwrap();
        assert_eq!(entry.root, root);
        assert_eq!(entry.leaf_hashes, leaf_hashes);
    }

    #[test]
    fn merkle_cache_stale_on_mtime_change() {
        let db = Database::open_in_memory().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        fs::write(&file_path, b"hello").unwrap();

        // Store a merkle entry
        let root = Blake3Hash::new(b"test");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &root,
            &leaf_hashes,
        )
        .unwrap();

        // Modify the file (change mtime)
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&file_path, b"modified").unwrap();

        // Should be stale now
        let result = db.get_merkle(&file_path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn merkle_cache_stale_on_size_change() {
        let db = Database::open_in_memory().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        fs::write(&file_path, b"hello").unwrap();

        // Store a merkle entry
        let root = Blake3Hash::new(b"test");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            file_mtime_ns(&file_path),
            file_size(&file_path),
            &root,
            &leaf_hashes,
        )
        .unwrap();

        // Change the file size
        fs::write(&file_path, b"much longer content").unwrap();

        // Should be stale now
        let result = db.get_merkle(&file_path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn merkle_cache_persists_across_reopen() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let file_path = temp_dir.path().join("test.txt");

        fs::write(&file_path, b"hello").unwrap();

        // Create database and store entry
        let root = Blake3Hash::new(b"test");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        {
            let db = Database::open(&db_path).unwrap();
            db.put_merkle(
                &file_path,
                file_mtime_ns(&file_path),
                file_size(&file_path),
                &root,
                &leaf_hashes,
            )
            .unwrap();
        } // db dropped, file closed

        // Reopen and retrieve
        {
            let db = Database::open(&db_path).unwrap();
            let result = db.get_merkle(&file_path).unwrap();
            assert!(result.is_some());

            let entry = result.unwrap();
            assert_eq!(entry.root, root);
        }
    }
}
