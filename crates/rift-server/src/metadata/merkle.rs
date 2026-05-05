//! Merkle tree cache operations.
//!
//! Stores and retrieves Merkle tree data keyed by file path and metadata.

use crate::metadata::db::Database;
use rift_common::crypto::{Blake3Hash, LeafInfo, MerkleChild, MerkleTree};
use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;
use tokio_rusqlite::rusqlite;
use tokio_rusqlite::Result as SqliteResult;

/// Status of a file's Merkle cache entry.
///
/// Returned by [`Database::cache_status`] — a pure-DB check that accepts
/// `mtime_ns` and `file_size` from the caller so it has no filesystem I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    /// Cache entry exists with matching key + tree nodes + leaf info.
    Complete,
    /// Cache entry exists but mtime/size differ from the supplied key.
    Stale,
    /// No cache entry at all for this path.
    Missing,
    /// Cache entry exists with matching key but `tree_nodes` or `leaf_info` are absent.
    Incomplete,
}

/// Check if an error is `QueryReturnedNoRows`.
///
/// This helper avoids fragile string matching and uses proper
/// enum variant matching for the `tokio_rusqlite` error wrapper.
fn is_no_rows(e: &tokio_rusqlite::Error) -> bool {
    matches!(
        e,
        tokio_rusqlite::Error::Error(rusqlite::Error::QueryReturnedNoRows)
    )
}

/// A cached Merkle tree entry.
#[derive(Debug, Clone, PartialEq)]
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
    /// - The filesystem mtime is unavailable (cannot validate freshness)
    pub async fn get_merkle(&self, path: &Path) -> SqliteResult<Option<MerkleEntry>> {
        use std::fs;

        let Ok(meta) = fs::metadata(path) else {
            return Ok(None);
        };

        let mtime_ns = meta.modified().ok().and_then(|t| {
            t.duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|d| u64::try_from(d.as_nanos()).ok())
        });

        let file_size = meta.len();
        let path_str = path.to_string_lossy().to_string();

        // If mtime is unavailable, we cannot validate cache freshness.
        // Log a warning and return None to avoid incorrect cache hits.
        let Some(mtime_ns) = mtime_ns else {
            tracing::warn!(
                path = %path_str,
                "file mtime unavailable; treating cache entry as stale"
            );
            return Ok(None);
        };

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
                            row.get::<_, Option<i64>>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
            })
            .await;

        match result {
            Ok((root, leaf_hashes, cached_mtime, cached_size)) => {
                // Compare mtime: both must be Some and equal, and size must match.
                let mtime_matches = cached_mtime.map(|m| m == mtime_ns as i64).unwrap_or(false);
                if mtime_matches && cached_size == file_size as i64 {
                    let Ok(root) = Blake3Hash::from_slice(&root) else {
                        return Ok(None);
                    };
                    let Ok(leaf_hashes) = MerkleTree::default().deserialize_leaves(&leaf_hashes)
                    else {
                        return Ok(None);
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
            Err(e) if is_no_rows(&e) => Ok(None),
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
    /// Takes explicit `mtime_ns` and `file_size` rather than reading from the
    /// filesystem, so callers can pass verified/cached values.
    ///
    /// `mtime_ns` is `Option<u64>`:
    /// - `None` means unknown/unavailable mtime (will store NULL in database)
    /// - `Some(0)` means actual Unix epoch timestamp
    pub async fn put_merkle(
        &self,
        path: &Path,
        mtime_ns: Option<u64>,
        file_size: u64,
        root: &Blake3Hash,
        leaf_hashes: &[Blake3Hash],
    ) -> SqliteResult<()> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()) as i64;

        let merkle = MerkleTree::default();
        let serialized_leaves = merkle.serialize_leaves(leaf_hashes);

        let path_str = path.to_string_lossy().to_string();
        let root_bytes = root.as_bytes().to_vec();
        let serialized = serialized_leaves;

        // Truncation only possible if usize > i64::MAX, which cannot happen for leaf counts.
        let leaf_count = leaf_hashes.len() as i64;

        let mtime_ns_i64 = mtime_ns.map(|ns| ns as i64);

        self.call(move |conn: &mut rusqlite::Connection| {
            conn.execute(
                "INSERT OR REPLACE INTO merkle_cache
                 (file_path, mtime_ns, file_size, root_hash, leaf_hashes, leaf_count, computed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                (
                    path_str,
                    mtime_ns_i64,
                    file_size as i64,
                    root_bytes,
                    serialized,
                    leaf_count,
                    now,
                ),
            )
        })
        .await?;

        Ok(())
    }

    /// Store a complete Merkle tree for a file, including intermediate
    /// nodes and leaf metadata.
    ///
    /// This populates the `merkle_tree_nodes` and `merkle_leaf_info` tables
    /// and also updates the `merkle_cache` table for backward compatibility.
    /// The operation is atomic (wrapped in a transaction).
    ///
    /// `mtime_ns` is `Option<u64>`:
    /// - `None` means unknown/unavailable mtime (will store NULL in database)
    /// - `Some(0)` means actual Unix epoch timestamp
    pub async fn put_tree(
        &self,
        path: &Path,
        mtime_ns: Option<u64>,
        file_size: u64,
        root: &Blake3Hash,
        cache: &HashMap<Blake3Hash, Vec<MerkleChild>>,
        leaf_infos: &[LeafInfo],
    ) -> SqliteResult<()> {
        let path_str = path.to_string_lossy().to_string();

        // Serialize all children entries for DB insertion
        let mut node_entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(cache.len());
        for (hash, children) in cache {
            let hash_bytes = hash.as_bytes().to_vec();
            let children_bytes = postcard::to_allocvec(children)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            node_entries.push((hash_bytes, children_bytes));
        }

        // Serialize leaf info entries
        let mut leaf_entries: Vec<(Vec<u8>, i64, i64, i64)> = Vec::with_capacity(leaf_infos.len());
        for info in leaf_infos {
            let hash_bytes = info.hash.as_bytes().to_vec();
            leaf_entries.push((
                hash_bytes,
                info.offset as i64,
                info.length as i64,
                info.chunk_index.into(),
            ));
        }

        let root_bytes = root.as_bytes().to_vec();
        let merkle = MerkleTree::default();
        let leaf_hashes_for_cache: Vec<Blake3Hash> =
            leaf_infos.iter().map(|info| info.hash.clone()).collect();
        let serialized_leaves = merkle.serialize_leaves(&leaf_hashes_for_cache);
        // Truncation only possible if usize > i64::MAX, which cannot happen for leaf counts.
        let leaf_count = leaf_infos.len() as i64;

        let mtime_ns_i64 = mtime_ns.map(|ns| ns as i64);

        self.call(move |conn: &mut rusqlite::Connection| {
            let tx = conn.transaction()?;

            // Delete old data for this file
            tx.execute(
                "DELETE FROM merkle_tree_nodes WHERE file_path = ?1",
                [&path_str],
            )?;
            tx.execute(
                "DELETE FROM merkle_leaf_info WHERE file_path = ?1",
                [&path_str],
            )?;

            // Insert all node entries
            for (hash_bytes, children_bytes) in &node_entries {
                tx.execute(
                    "INSERT INTO merkle_tree_nodes (file_path, node_hash, children) VALUES (?1, ?2, ?3)",
                    rusqlite::params![path_str, hash_bytes, children_bytes],
                )?;
            }

            // Insert all leaf info entries
            for (hash_bytes, offset, length, chunk_index) in &leaf_entries {
                tx.execute(
                    "INSERT INTO merkle_leaf_info (file_path, chunk_hash, chunk_offset, chunk_length, chunk_index) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![path_str, hash_bytes, offset, length, chunk_index],
                )?;
            }

            // Also update the legacy merkle_cache table
            {
                use std::time::{SystemTime, UNIX_EPOCH};
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()) as i64;

                tx.execute(
                    "INSERT OR REPLACE INTO merkle_cache (file_path, mtime_ns, file_size, root_hash, leaf_hashes, leaf_count, computed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![path_str, mtime_ns_i64, file_size as i64, root_bytes, serialized_leaves, leaf_count, now],
                )?;
            }

            tx.commit()?;
            Ok(())
        })
        .await?;

        Ok(())
    }

    /// Look up the children of a Merkle tree node by its hash.
    ///
    /// Returns `None` if the hash is not found or if deserialization fails.
    pub async fn get_children(
        &self,
        path: &Path,
        node_hash: &Blake3Hash,
    ) -> SqliteResult<Option<Vec<MerkleChild>>> {
        let path_str = path.to_string_lossy().to_string();
        let hash_bytes = node_hash.as_bytes().to_vec();

        let result = self
            .call(move |conn: &mut rusqlite::Connection| {
                conn.query_row(
                    "SELECT children FROM merkle_tree_nodes WHERE file_path = ?1 AND node_hash = ?2",
                    rusqlite::params![path_str, hash_bytes],
                    |row| row.get::<_, Vec<u8>>(0),
                )
            })
            .await;

        match result {
            Ok(children_blob) => {
                let children: Vec<MerkleChild> = match postcard::from_bytes(&children_blob) {
                    Ok(c) => c,
                    Err(_) => return Ok(None), // Graceful degradation on corrupt data
                };
                Ok(Some(children))
            }
            Err(e) => {
                if is_no_rows(&e) {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Look up all leaf metadata for a file, ordered by chunk index.
    ///
    /// Returns `None` if no leaf info exists for the path.
    pub async fn get_all_leaf_info(&self, path: &Path) -> SqliteResult<Option<Vec<LeafInfo>>> {
        let path_str = path.to_string_lossy().to_string();

        let result = self
            .call(move |conn: &mut rusqlite::Connection| {
                let mut stmt = conn.prepare(
                    "SELECT chunk_hash, chunk_offset, chunk_length, chunk_index
                     FROM merkle_leaf_info
                     WHERE file_path = ?1
                     ORDER BY chunk_index ASC",
                )?;
                let rows = stmt.query_map([path_str], |row| {
                    let hash_bytes: Vec<u8> = row.get(0)?;
                    let offset: i64 = row.get(1)?;
                    let length: i64 = row.get(2)?;
                    let chunk_index: i64 = row.get(3)?;
                    let Ok(hash) = Blake3Hash::from_slice(&hash_bytes) else {
                        return Err(rusqlite::Error::IntegralValueOutOfRange(0, 0));
                    };
                    Ok(LeafInfo {
                        hash,
                        offset: offset as u64,
                        length: length as u64,
                        chunk_index: u32::try_from(chunk_index)
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, 0))?,
                    })
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await;

        match result {
            Ok(infos) => {
                if infos.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(infos))
                }
            }
            Err(e) => {
                if is_no_rows(&e) {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Delete all cached Merkle data for a file.
    pub async fn delete_merkle(&self, path: &Path) -> SqliteResult<()> {
        let path_str = path.to_string_lossy().to_string();
        self.call(move |conn: &mut rusqlite::Connection| {
            conn.execute("DELETE FROM merkle_cache WHERE file_path = ?1", [&path_str])?;
            conn.execute(
                "DELETE FROM merkle_tree_nodes WHERE file_path = ?1",
                [&path_str],
            )?;
            conn.execute(
                "DELETE FROM merkle_leaf_info WHERE file_path = ?1",
                [&path_str],
            )?;
            Ok(())
        })
        .await
    }

    /// Determine the cache status for a given file path.
    ///
    /// Purely database-based check — caller must supply current filesystem
    /// `mtime_ns` and `file_size` so the method has no I/O coupling.
    ///
    /// Single SQL round-trip using `EXISTS` subqueries.
    ///
    /// `mtime_ns` is `Option<u64>`:
    /// - `None` means unknown mtime - cannot validate cache freshness, return Stale/Missing
    /// - `Some(value)` means known mtime - can compare with cache
    pub async fn cache_status(
        &self,
        path: &Path,
        mtime_ns: Option<u64>,
        file_size: u64,
    ) -> SqliteResult<CacheStatus> {
        // If mtime is unknown, we cannot validate cache freshness.
        // Return Missing if no entry exists, Stale otherwise.
        let Some(mtime_ns) = mtime_ns else {
            let path_str = path.to_string_lossy().to_string();
            let any_entry: bool = self
                .call(move |conn| {
                    conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM merkle_cache WHERE file_path = ?1)",
                        rusqlite::params![path_str],
                        |row| row.get::<_, bool>(0),
                    )
                })
                .await?;
            return Ok(if any_entry {
                CacheStatus::Stale
            } else {
                CacheStatus::Missing
            });
        };

        let path_str = path.to_string_lossy().to_string();
        self.call(move |conn| {
            // Try to get leaf_count from a cache entry with matching key
            let cached_leaf_count_result = conn.query_row(
                "SELECT leaf_count FROM merkle_cache WHERE file_path = ?1 AND mtime_ns = ?2 AND file_size = ?3",
                rusqlite::params![path_str, mtime_ns as i64, file_size as i64],
                |row| row.get::<_, i64>(0),
            );

            let cached_leaf_count = match cached_leaf_count_result {
                Ok(count) => count,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    // Check if an entry exists with WRONG key → Stale
                    let any_entry: bool = conn
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM merkle_cache WHERE file_path = ?1)",
                            rusqlite::params![path_str],
                            |row| row.get::<_, bool>(0),
                        )?;
                    return Ok(if any_entry { CacheStatus::Stale } else { CacheStatus::Missing });
                }
                Err(e) => return Err(e),
            };

            // Cache key matches — check completeness
            // For non-empty files, tree nodes are required
            if cached_leaf_count > 0 {
                let has_nodes: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM merkle_tree_nodes WHERE file_path = ?1)",
                        rusqlite::params![path_str],
                        |row| row.get::<_, bool>(0),
                    )?;

                if !has_nodes {
                    return Ok(CacheStatus::Incomplete);
                }
            }

            let actual_leaf_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM merkle_leaf_info WHERE file_path = ?1",
                    rusqlite::params![path_str],
                    |row| row.get::<_, i64>(0),
                )?;

            if actual_leaf_count == cached_leaf_count {
                Ok(CacheStatus::Complete)
            } else {
                Ok(CacheStatus::Incomplete)
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation)]
    use super::*;
    use rift_common::crypto::{Blake3Hash, LeafInfo, MerkleTree};
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    // =======================================================================
    // cache_status tests
    // =======================================================================

    fn file_mtime_ns(path: &Path) -> Option<u64> {
        let meta = fs::metadata(path).unwrap();
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok())
    }

    fn file_size(path: &Path) -> u64 {
        fs::metadata(path).unwrap().len()
    }

    #[tokio::test]
    async fn cache_status_returns_missing_for_no_entry() {
        let db = Database::open_in_memory().await.unwrap();
        let path = Path::new("/tmp/nonexistent.txt");
        let result = db.cache_status(path, None, 0).await.unwrap();
        assert_eq!(result, CacheStatus::Missing, "no entry → Missing");
    }

    #[tokio::test]
    async fn cache_status_returns_complete_for_fully_cached_file() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("complete.txt");
        fs::write(&file_path, b"hello").unwrap();

        let root = Blake3Hash::new(b"root");
        let leaf = Blake3Hash::new(b"leaf");
        let mtime_ns = file_mtime_ns(&file_path);
        let size = file_size(&file_path);
        let tree = MerkleTree::default();
        let leaf_infos = vec![LeafInfo {
            hash: leaf.clone(),
            offset: 0,
            length: 5,
            chunk_index: 0,
        }];
        let cache = tree.build_with_cache(std::slice::from_ref(&leaf)).1;

        db.put_tree(&file_path, mtime_ns, size, &root, &cache, &leaf_infos)
            .await
            .unwrap();

        let result = db.cache_status(&file_path, mtime_ns, size).await.unwrap();
        assert_eq!(result, CacheStatus::Complete, "fully cached → Complete");

        // Verify leaf_count was stored correctly
        let path_str = file_path.to_string_lossy().to_string();
        let stored_leaf_count: i64 = db
            .call(move |conn| {
                conn.query_row(
                    "SELECT leaf_count FROM merkle_cache WHERE file_path = ?1",
                    [path_str],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(
            stored_leaf_count, 1,
            "leaf_count should match number of leaf_infos"
        );
    }

    #[tokio::test]
    async fn cache_status_returns_stale_for_wrong_mtime() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("stale_mtime.txt");
        fs::write(&file_path, b"hello").unwrap();

        let root = Blake3Hash::new(b"root");
        let leaf = Blake3Hash::new(b"leaf");
        let stale_mtime = Some(0u64); // Store with epoch mtime
        let size = file_size(&file_path);

        // Store with stale mtime
        db.put_merkle(&file_path, stale_mtime, size, &root, &[leaf])
            .await
            .unwrap();

        // Query with the CORRECT mtime → should see Stale (entry exists but key differs)
        let real_mtime = file_mtime_ns(&file_path);
        let result = db.cache_status(&file_path, real_mtime, size).await.unwrap();
        assert_eq!(result, CacheStatus::Stale, "wrong mtime → Stale");
    }

    #[tokio::test]
    async fn cache_status_returns_stale_for_wrong_size() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("stale_size.txt");
        fs::write(&file_path, b"hello").unwrap();

        let root = Blake3Hash::new(b"root");
        let leaf = Blake3Hash::new(b"leaf");
        let mtime_ns = file_mtime_ns(&file_path);
        let stale_size = 999u64;

        // Store with stale size
        db.put_merkle(&file_path, mtime_ns, stale_size, &root, &[leaf])
            .await
            .unwrap();

        // Query with the CORRECT size → should see Stale
        let real_size = file_size(&file_path);
        let result = db
            .cache_status(&file_path, mtime_ns, real_size)
            .await
            .unwrap();
        assert_eq!(result, CacheStatus::Stale, "wrong size → Stale");
    }

    #[tokio::test]
    async fn cache_status_returns_incomplete_for_missing_tree_nodes() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("missing_nodes.txt");
        fs::write(&file_path, b"hello").unwrap();

        let root = Blake3Hash::new(b"root");
        let leaf = Blake3Hash::new(b"leaf");
        let mtime_ns = file_mtime_ns(&file_path);
        let size = file_size(&file_path);

        // Only put the merkle_cache row, NOT merkle_tree_nodes or merkle_leaf_info
        db.put_merkle(&file_path, mtime_ns, size, &root, &[leaf])
            .await
            .unwrap();

        let result = db.cache_status(&file_path, mtime_ns, size).await.unwrap();
        assert_eq!(
            result,
            CacheStatus::Incomplete,
            "missing tree nodes → Incomplete"
        );
    }

    #[tokio::test]
    async fn cache_status_returns_incomplete_for_missing_leaf_info() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("missing_leaf_info.txt");
        fs::write(&file_path, b"hello").unwrap();

        let root = Blake3Hash::new(b"root");
        let leaf = Blake3Hash::new(b"leaf");
        let mtime_ns = file_mtime_ns(&file_path);
        let size = file_size(&file_path);
        let tree = MerkleTree::default();
        let cache = tree.build_with_cache(std::slice::from_ref(&leaf)).1;

        // Put all three, then manually delete leaf_info
        let leaf_infos = vec![LeafInfo {
            hash: leaf.clone(),
            offset: 0,
            length: 5,
            chunk_index: 0,
        }];
        db.put_tree(&file_path, mtime_ns, size, &root, &cache, &leaf_infos)
            .await
            .unwrap();

        let path_str = file_path.to_string_lossy().to_string();
        db.call(move |conn| {
            conn.execute(
                "DELETE FROM merkle_leaf_info WHERE file_path = ?1",
                [path_str],
            )
        })
        .await
        .unwrap();

        let result = db.cache_status(&file_path, mtime_ns, size).await.unwrap();
        assert_eq!(
            result,
            CacheStatus::Incomplete,
            "missing leaf info → Incomplete"
        );
    }

    #[tokio::test]
    async fn cache_status_returns_complete_for_empty_file() {
        let db = Database::open_in_memory().await.unwrap();
        // Dummy path — no filesystem access needed, we only insert into the DB.
        let file_path = PathBuf::from("test_empty.txt");

        let root = Blake3Hash::new(b"root");
        let mtime_ns = Some(1000u64);
        let size = 0u64;

        // Insert merkle_cache row with leaf_count=0, no tree_nodes, no leaf_info
        let path_str = file_path.to_string_lossy().to_string();
        let root_bytes = root.as_bytes().to_vec();
        let mtime_ns_i64 = mtime_ns.map(|ns| ns as i64);
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO merkle_cache (file_path, mtime_ns, file_size, root_hash, leaf_hashes, leaf_count, computed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![path_str, mtime_ns_i64, size as i64, root_bytes, Vec::<u8>::new(), 0i64, 0i64],
            )
        }).await.unwrap();

        let result = db.cache_status(&file_path, mtime_ns, size).await.unwrap();
        assert_eq!(
            result,
            CacheStatus::Complete,
            "empty file with leaf_count=0 → Complete"
        );
    }

    #[tokio::test]
    async fn cache_status_returns_incomplete_for_wrong_leaf_count() {
        let db = Database::open_in_memory().await.unwrap();
        // Dummy path — no filesystem access needed, we only insert into the DB.
        let file_path = PathBuf::from("test_wrong_count.txt");

        let root = Blake3Hash::new(b"root");
        let mtime_ns = Some(2000u64);
        let size = 100u64;

        // Insert merkle_cache with leaf_count=3 but only 2 leaf_info rows
        let path_str = file_path.to_string_lossy().to_string();
        let root_bytes = root.as_bytes().to_vec();
        let mtime_ns_i64 = mtime_ns.map(|ns| ns as i64);
        db.call({
            let path_str = path_str.clone();
            move |conn| {
                conn.execute(
                    "INSERT INTO merkle_cache (file_path, mtime_ns, file_size, root_hash, leaf_hashes, leaf_count, computed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![path_str, mtime_ns_i64, size as i64, root_bytes, Vec::<u8>::new(), 3i64, 0i64],
                )?;
                // Insert a tree node so has_nodes is true
                conn.execute(
                    "INSERT INTO merkle_tree_nodes (file_path, node_hash, children) VALUES (?1, ?2, ?3)",
                    rusqlite::params![path_str, vec![0u8; 32], vec![1u8; 64]],
                )?;
                // Insert only 2 leaf_info rows
                for i in 0..2 {
                    conn.execute(
                        "INSERT INTO merkle_leaf_info (file_path, chunk_hash, chunk_offset, chunk_length, chunk_index) VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![path_str, vec![i as u8; 32], i * 100i64, 100i64, i],
                    )?;
                }
                Ok(())
            }
        }).await.unwrap();

        let result = db.cache_status(&file_path, mtime_ns, size).await.unwrap();
        assert_eq!(
            result,
            CacheStatus::Incomplete,
            "wrong leaf count → Incomplete"
        );
    }

    #[tokio::test]
    async fn put_tree_stores_leaf_count() {
        let db = Database::open_in_memory().await.unwrap();
        // Dummy path — no filesystem access needed, we only insert via put_tree.
        let file_path = PathBuf::from("test_leaf_count.txt");

        let tree = MerkleTree::default();
        let leaves = vec![Blake3Hash::new(b"chunk1"), Blake3Hash::new(b"chunk2")];
        let chunks = vec![(0usize, 11usize), (11usize, 11usize)];
        let (root, cache, leaf_infos) = tree.build_with_cache_and_offsets(&leaves, &chunks);

        let mtime_ns = Some(3000u64);
        let file_size = 22u64;

        db.put_tree(&file_path, mtime_ns, file_size, &root, &cache, &leaf_infos)
            .await
            .unwrap();

        let path_str = file_path.to_string_lossy().to_string();
        let stored_leaf_count: i64 = db
            .call(move |conn| {
                conn.query_row(
                    "SELECT leaf_count FROM merkle_cache WHERE file_path = ?1",
                    [path_str],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(
            stored_leaf_count, 2,
            "leaf_count should match number of leaf_infos"
        );
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

    // Unit tests for is_no_rows helper
    #[test]
    fn is_no_rows_detects_query_returned_no_rows() {
        let err = tokio_rusqlite::Error::Error(rusqlite::Error::QueryReturnedNoRows);
        assert!(is_no_rows(&err), "Should detect QueryReturnedNoRows");
    }

    #[test]
    fn is_no_rows_false_for_other_errors() {
        // Test various other error variants - using InvalidParameterCount
        let sql_err = rusqlite::Error::InvalidParameterCount(2, 3);
        let err = tokio_rusqlite::Error::Error(sql_err);
        assert!(!is_no_rows(&err), "Should not match InvalidParameterCount");

        let err = tokio_rusqlite::Error::ConnectionClosed;
        assert!(!is_no_rows(&err), "Should not match ConnectionClosed");
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
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("pre_epoch.txt");
        fs::write(&file_path, b"test content").unwrap();

        // Set mtime before Unix epoch (1969-12-31) using safe std::fs API
        let pre_epoch = std::time::SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(86400);
        filetime::set_file_mtime(&file_path, pre_epoch.into()).unwrap();

        // Should not panic - function must complete without panic
        let db = Database::open_in_memory().await.unwrap();
        let _result = db.get_merkle(&file_path).await; // If panic occurs, test fails here
    }

    #[tokio::test]
    async fn put_tree_and_get_children_root() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, b"hello world").unwrap();

        // Build a small tree: 2 leaves
        let tree = MerkleTree::default();
        let leaves = vec![Blake3Hash::new(b"chunk1"), Blake3Hash::new(b"chunk2")];
        let chunks = vec![(0usize, 5usize), (5usize, 6usize)]; // fake chunk boundaries
        let (root, cache, leaf_infos) = tree.build_with_cache_and_offsets(&leaves, &chunks);

        let meta = std::fs::metadata(&file_path).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());

        db.put_tree(&file_path, mtime_ns, meta.len(), &root, &cache, &leaf_infos)
            .await
            .unwrap();

        // Query root's children
        let children = db.get_children(&file_path, &root).await.unwrap();
        assert!(children.is_some(), "Root should have children in DB");
        let children = children.unwrap();
        assert_eq!(children.len(), 2, "Root should have 2 children (2 leaves)");
    }

    #[tokio::test]
    async fn get_children_nonexistent_hash_returns_none() {
        let db = Database::open_in_memory().await.unwrap();
        let file_path = std::path::PathBuf::from("/tmp/nonexistent.txt");
        let fake_hash = Blake3Hash::new(b"does not exist");
        let result = db.get_children(&file_path, &fake_hash).await.unwrap();
        assert!(result.is_none(), "Non-existent hash should return None");
    }

    // =======================================================================
    // mtime_ns Option<u64> tests - distinguish unknown from epoch
    // =======================================================================

    /// Test that demonstrates the bug: `mtime_ns=0` from failure vs real epoch.
    ///
    /// When `modified()` fails or returns pre-epoch time, the code currently
    /// falls back to `mtime_ns=0`. But 0 is also a valid timestamp (Unix epoch).
    /// This conflation causes correctness issues.
    ///
    /// The fix: change `mtime_ns` to Option<u64>:
    /// - None means unknown (failure)
    /// - Some(0) means actual epoch timestamp
    #[tokio::test]
    async fn cache_status_distinguishes_unknown_mtime_from_epoch() {
        // This test demonstrates that we MUST distinguish between:
        // - Unknown mtime (None) - cache key is unknown
        // - Epoch mtime (Some(0)) - cache key is valid and equals 0
        //
        // Current implementation conflates both as 0, which is wrong.
        //
        // Scenario:
        // 1. File A has mtime=0 (actual epoch timestamp - unusual but valid)
        // 2. File B has mtime computation failure (should be None)
        //
        // With the bug: both would have mtime_ns=0 in cache
        // The fix: File A's cache entry has mtime_ns=Some(0), File B's has None
        //
        // For now, this test documents the expected behavior after the fix.
        // When mtime_ns is Option<u64>:
        // - put_tree with mtime_ns=Some(0) should store 0
        // - cache_status with mtime_ns=Some(0) should match
        // - put_tree with mtime_ns=None should store NULL
        // - cache_status with mtime_ns=None should treat as "unknown"

        let db = Database::open_in_memory().await.unwrap();
        let file_path = PathBuf::from("/test/epoch_file.txt");

        // Store with actual epoch mtime (Some(0))
        let root = Blake3Hash::new(b"epoch_root");
        let leaf = Blake3Hash::new(b"epoch_leaf");
        let tree = MerkleTree::default();
        let leaf_infos = vec![LeafInfo {
            hash: leaf.clone(),
            offset: 0,
            length: 5,
            chunk_index: 0,
        }];
        let cache = tree.build_with_cache(std::slice::from_ref(&leaf)).1;

        // mtime_ns = Some(0) means actual epoch timestamp - this should be stored
        db.put_tree(&file_path, Some(0), 5u64, &root, &cache, &leaf_infos)
            .await
            .unwrap();

        // Query with same mtime_ns=Some(0) should find the entry
        let result = db.cache_status(&file_path, Some(0), 5u64).await.unwrap();
        assert_eq!(
            result,
            CacheStatus::Complete,
            "epoch mtime (Some(0)) should match stored cache entry"
        );

        // Now test the unknown case: put with None should store NULL
        let unknown_path = PathBuf::from("/test/unknown_mtime.txt");
        let unknown_root = Blake3Hash::new(b"unknown_root");
        let unknown_leaf = Blake3Hash::new(b"unknown_leaf");
        let unknown_leaf_infos = vec![LeafInfo {
            hash: unknown_leaf.clone(),
            offset: 0,
            length: 10,
            chunk_index: 0,
        }];
        let unknown_cache = tree.build_with_cache(std::slice::from_ref(&unknown_leaf)).1;

        // mtime_ns = None means unknown/failure
        db.put_tree(
            &unknown_path,
            None,
            10u64,
            &unknown_root,
            &unknown_cache,
            &unknown_leaf_infos,
        )
        .await
        .unwrap();

        // Query with mtime_ns=None should still find the entry (we match on path+size when mtime is unknown)
        let result = db.cache_status(&unknown_path, None, 10u64).await.unwrap();
        // Expected: Missing because unknown mtime can never match (no filesystem truth to compare against)
        // OR: Complete if we decide that unknown matches unknown
        // The design decision: unknown mtime means we cannot validate cache freshness, so it's always stale/invalid
        assert_eq!(
            result,
            CacheStatus::Stale,
            "unknown mtime should result in Stale (cannot validate cache freshness without mtime)"
        );
    }

    #[tokio::test]
    async fn put_tree_with_duplicate_chunk_hashes() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("duplicate_chunks.txt");
        std::fs::write(&file_path, b"AAAAAA").unwrap();

        let chunk_hash = Blake3Hash::new(b"same_hash");
        let leaf_infos = vec![
            LeafInfo {
                hash: chunk_hash.clone(),
                offset: 0,
                length: 3,
                chunk_index: 0,
            },
            LeafInfo {
                hash: chunk_hash.clone(),
                offset: 3,
                length: 3,
                chunk_index: 1,
            },
        ];
        let root = Blake3Hash::new(b"root");
        let cache = HashMap::new();

        let meta = std::fs::metadata(&file_path).unwrap();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok());

        // put_tree should succeed even when two leaves have the same hash
        db.put_tree(&file_path, mtime_ns, meta.len(), &root, &cache, &leaf_infos)
            .await
            .unwrap();

        // Verify leaf_count is 2
        let path_str = file_path.to_string_lossy().to_string();
        let stored_leaf_count: i64 = db
            .call(move |conn| {
                conn.query_row(
                    "SELECT leaf_count FROM merkle_cache WHERE file_path = ?1",
                    [path_str],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(
            stored_leaf_count, 2,
            "leaf_count should be 2 for file with duplicate chunk hashes"
        );

        // Verify both leaf_info rows exist
        let all_leaf_info = db.get_all_leaf_info(&file_path).await.unwrap();
        assert!(all_leaf_info.is_some(), "should have leaf_info");
        let infos = all_leaf_info.unwrap();
        assert_eq!(infos.len(), 2, "both leaf entries should exist");
        assert_eq!(infos[0].chunk_index, 0);
        assert_eq!(infos[1].chunk_index, 1);
        assert_eq!(infos[0].hash, chunk_hash);
        assert_eq!(infos[1].hash, chunk_hash);
    }

    #[tokio::test]
    async fn get_merkle_treats_null_mtime_as_stale() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("null_mtime.txt");
        fs::write(&file_path, b"hello").unwrap();

        let root = Blake3Hash::new(b"test");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        // Store with mtime_ns = None (NULL in DB)
        db.put_merkle(&file_path, None, file_size(&file_path), &root, &leaf_hashes)
            .await
            .unwrap();

        // get_merkle should return None (stale) rather than erroring
        let result = db.get_merkle(&file_path).await;
        assert!(
            result.is_ok(),
            "get_merkle should not error on NULL mtime: {:?}",
            result.err()
        );
        assert_eq!(
            result.unwrap(),
            None,
            "NULL mtime should be treated as stale"
        );
    }

    #[tokio::test]
    async fn get_merkle_matches_epoch_mtime() {
        let db = Database::open_in_memory().await.unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("epoch_mtime.txt");
        fs::write(&file_path, b"hello").unwrap();

        // Set mtime to Unix epoch (0)
        let epoch = std::time::SystemTime::UNIX_EPOCH;
        filetime::set_file_mtime(&file_path, epoch.into()).unwrap();

        let root = Blake3Hash::new(b"test");
        let leaf_hashes = vec![Blake3Hash::new(b"chunk1")];
        db.put_merkle(
            &file_path,
            Some(0),
            file_size(&file_path),
            &root,
            &leaf_hashes,
        )
        .await
        .unwrap();

        let result = db.get_merkle(&file_path).await.unwrap();
        assert!(
            result.is_some(),
            "epoch mtime (Some(0)) should match stored cache entry"
        );
        let entry = result.unwrap();
        assert_eq!(entry.root, root);
        assert_eq!(entry.leaf_hashes, leaf_hashes);
    }
}
