//! SQLite-based file cache.
//!
//! Stores:
//! - File manifests: (handle -> root_hash, chunk list) — in SQLite
//! - Chunk data: (chunk_hash -> data) — on disk via ChunkStore
//!
//! TODO(v1): Implement configurable cache size limits per mount.
//! Current: unlimited. Future: LRU eviction based on configurable budget.
//! See docs/03-cli-design/commands.md for planned config:
//!   rift config get client.cache_size
//!   rift config set client.cache_size 2GB

use rift_common::crypto::Blake3Hash;
use std::path::Path;
use tokio_rusqlite::rusqlite;
use tokio_rusqlite::Connection;
use tokio_rusqlite::Result as SqliteResult;
use uuid::Uuid;

/// A file manifest mapping a server handle to its Merkle tree.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// The root hash of the file's Merkle tree
    pub root: Blake3Hash,
    /// The list of chunks (offset, length, hash)
    pub chunks: Vec<ChunkInfo>,
}

/// Information about a single chunk in a file.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkInfo {
    /// Position in the file (0-indexed)
    pub index: u32,
    /// Byte offset in the file
    pub offset: u64,
    /// Chunk length in bytes
    pub length: u64,
    /// BLAKE3 hash of the chunk content
    pub hash: [u8; 32],
}

/// A file cache for storing root hashes and chunk data.
///
/// Metadata (manifests, chunk references) is stored in SQLite.
/// Chunk data is stored on disk via `ChunkStore`.
pub struct FileCache {
    conn: Connection,
    chunk_store: Option<crate::cache::chunks::ChunkStore>,
}

impl FileCache {
    /// Open a cache at the given directory path.
    ///
    /// Creates the directory and database if they don't exist.
    /// Chunk data is stored under `cache_dir/chunks/` via `ChunkStore`.
    pub async fn open(cache_dir: &Path) -> SqliteResult<Self> {
        std::fs::create_dir_all(cache_dir).ok();
        let db_path = cache_dir.join("cache.db");
        let conn = Connection::open(db_path).await?;

        conn.call(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA foreign_keys=ON;

                 CREATE TABLE IF NOT EXISTS manifests (
                     handle       TEXT PRIMARY KEY,
                     root_hash   BLOB NOT NULL,
                     updated_at  INTEGER NOT NULL
                 );

                 CREATE TABLE IF NOT EXISTS chunk_refs (
                     handle       TEXT NOT NULL,
                     chunk_index INTEGER NOT NULL,
                     byte_offset INTEGER NOT NULL,
                     byte_length INTEGER NOT NULL,
                     chunk_hash  BLOB NOT NULL,
                     PRIMARY KEY (handle, chunk_index),
                     FOREIGN KEY (handle) REFERENCES manifests(handle) ON DELETE CASCADE
                 );

                 CREATE INDEX IF NOT EXISTS idx_chunk_refs_hash ON chunk_refs(chunk_hash);
                 CREATE INDEX IF NOT EXISTS idx_chunk_refs_handle ON chunk_refs(handle);",
            )
        })
        .await?;

        let chunk_store = crate::cache::chunks::ChunkStore::open(cache_dir)
            .await
            .map_err(|e| tokio_rusqlite::rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

        Ok(Self {
            conn,
            chunk_store: Some(chunk_store),
        })
    }

    /// Open an in-memory cache for testing (manifest operations only).
    ///
    /// No chunk storage is available — `put_chunk`/`get_chunk`/`reconstruct`
    /// will panic if called on an in-memory cache.
    #[cfg(test)]
    pub async fn open_in_memory() -> SqliteResult<Self> {
        let conn = Connection::open_in_memory().await?;

        conn.call(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS manifests (
                     handle       TEXT PRIMARY KEY,
                     root_hash   BLOB NOT NULL,
                     updated_at  INTEGER NOT NULL
                 );

                 CREATE TABLE IF NOT EXISTS chunk_refs (
                     handle       TEXT NOT NULL,
                     chunk_index INTEGER NOT NULL,
                     byte_offset INTEGER NOT NULL,
                     byte_length INTEGER NOT NULL,
                     chunk_hash  BLOB NOT NULL,
                     PRIMARY KEY (handle, chunk_index)
                 );

                 CREATE INDEX IF NOT EXISTS idx_chunk_refs_hash ON chunk_refs(chunk_hash);",
            )
        })
        .await?;

        Ok(Self {
            conn,
            chunk_store: None,
        })
    }

    async fn call<F, R>(&self, f: F) -> tokio_rusqlite::Result<R>
    where
        F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        self.conn.call(f).await
    }

    /// Store the root hash for a file handle.
    pub async fn put_root_hash(&self, handle: &Uuid, root_hash: &Blake3Hash) -> SqliteResult<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let handle_str = handle.to_string();
        let root_bytes = root_hash.as_bytes().to_vec();

        self.call(move |conn: &mut rusqlite::Connection| {
            conn.execute(
                "INSERT OR REPLACE INTO manifests (handle, root_hash, updated_at) VALUES (?1, ?2, ?3)",
                (&handle_str, &root_bytes, now),
            )
        })
        .await?;
        Ok(())
    }

    /// Get the cached root hash for a file handle.
    pub async fn get_root_hash(&self, handle: &Uuid) -> SqliteResult<Option<Blake3Hash>> {
        let handle_str = handle.to_string();

        let result = self
            .call(move |conn: &mut rusqlite::Connection| {
                conn.query_row(
                    "SELECT root_hash FROM manifests WHERE handle = ?1",
                    [&handle_str],
                    |row| row.get::<_, Vec<u8>>(0),
                )
            })
            .await;

        match result {
            Ok(bytes) => Ok(Blake3Hash::from_slice(&bytes).ok()),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("QueryReturnedNoRows")
                    || err_str.contains("No such")
                    || err_str.contains("returned no rows")
                    || err_str.contains("not found")
                {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Store a file manifest (root hash + chunk list).
    pub async fn put_manifest(&self, handle: &Uuid, manifest: &Manifest) -> SqliteResult<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let handle_str = handle.to_string();
        let root_bytes = manifest.root.as_bytes().to_vec();
        let chunks: Vec<(i64, i64, i64, Vec<u8>)> = manifest
            .chunks
            .iter()
            .map(|c| {
                (
                    c.index as i64,
                    c.offset as i64,
                    c.length as i64,
                    c.hash.to_vec(),
                )
            })
            .collect();

        self.call(move |conn: &mut rusqlite::Connection| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT OR REPLACE INTO manifests (handle, root_hash, updated_at) VALUES (?1, ?2, ?3)",
                (&handle_str, &root_bytes, now),
            )?;

            for (index, offset, length, hash) in &chunks {
                tx.execute(
                    "INSERT OR REPLACE INTO chunk_refs (handle, chunk_index, byte_offset, byte_length, chunk_hash)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    (&handle_str, index, offset, length, hash),
                )?;
            }

            tx.commit()
        })
        .await?;
        Ok(())
    }

    /// Remove a file manifest and its chunk references from the cache.
    pub async fn remove_manifest(&self, handle: &Uuid) -> SqliteResult<()> {
        let handle_str = handle.to_string();
        self.call(move |conn: &mut rusqlite::Connection| {
            conn.execute("DELETE FROM chunk_refs WHERE handle = ?1", [&handle_str])?;
            conn.execute("DELETE FROM manifests WHERE handle = ?1", [&handle_str])?;
            Ok(())
        })
        .await
    }

    /// Get the cached manifest for a file handle.
    pub async fn get_manifest(&self, handle: &Uuid) -> SqliteResult<Option<Manifest>> {
        let handle_str = handle.to_string();
        let handle_str2 = handle_str.clone();

        let root_bytes: Option<Vec<u8>> = self
            .call(move |conn: &mut rusqlite::Connection| {
                match conn.query_row(
                    "SELECT root_hash FROM manifests WHERE handle = ?1",
                    [&handle_str],
                    |row| row.get::<_, Vec<u8>>(0),
                ) {
                    Ok(r) => Ok(Some(r)),
                    Err(e) => {
                        let err_str = e.to_string();
                        if err_str.contains("QueryReturnedNoRows")
                            || err_str.contains("No such")
                            || err_str.contains("returned no rows")
                            || err_str.contains("not found")
                        {
                            Ok(None)
                        } else {
                            Err(e)
                        }
                    }
                }
            })
            .await?;

        let root = match root_bytes {
            Some(bytes) => match Blake3Hash::from_slice(&bytes) {
                Ok(h) => h,
                Err(_) => return Ok(None),
            },
            None => return Ok(None),
        };

        let chunks: Vec<ChunkInfo> = self
            .call(move |conn: &mut rusqlite::Connection| {
                let mut stmt = conn.prepare(
                    "SELECT chunk_index, byte_offset, byte_length, chunk_hash
                     FROM chunk_refs WHERE handle = ?1 ORDER BY chunk_index",
                )?;

                let rows = stmt.query_map([&handle_str2], |row| {
                    let hash_bytes: Vec<u8> = row.get(3)?;
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(&hash_bytes);
                    Ok(ChunkInfo {
                        index: row.get::<_, i64>(0)? as u32,
                        offset: row.get::<_, i64>(1)? as u64,
                        length: row.get::<_, i64>(2)? as u64,
                        hash,
                    })
                })?;

                let mut result = Vec::new();
                for chunk in rows {
                    result.push(chunk?);
                }
                Ok(result)
            })
            .await?;

        Ok(Some(Manifest { root, chunks }))
    }

    /// Store chunk data keyed by its hash.
    ///
    /// Delegates to `ChunkStore::write_chunk`. Panics if the cache was
    /// opened in-memory (no chunk storage available).
    pub async fn put_chunk(&self, hash: &[u8; 32], data: &[u8]) -> SqliteResult<()> {
        let store = self
            .chunk_store
            .as_ref()
            .expect("put_chunk requires a ChunkStore; use FileCache::open(), not open_in_memory()");
        store
            .write_chunk(hash, data)
            .await
            .map_err(|e| tokio_rusqlite::rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        Ok(())
    }

    /// Get chunk data by its hash.
    ///
    /// Delegates to `ChunkStore::read_chunk`. Panics if the cache was
    /// opened in-memory (no chunk storage available).
    pub async fn get_chunk(&self, hash: &[u8; 32]) -> SqliteResult<Option<Vec<u8>>> {
        let store = self
            .chunk_store
            .as_ref()
            .expect("get_chunk requires a ChunkStore; use FileCache::open(), not open_in_memory()");
        Ok(store
            .read_chunk(hash)
            .await
            .map_err(|e| tokio_rusqlite::rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?)
    }

    /// Reconstruct a file from cached chunks, verifying each chunk's hash.
    ///
    /// Returns `Err` listing the hashes of missing or corrupted chunks.
    /// Panics if the cache was opened in-memory (no chunk storage available).
    pub async fn reconstruct(&self, chunks: &[ChunkInfo]) -> Result<Vec<u8>, Vec<[u8; 32]>> {
        let store = self.chunk_store.as_ref().expect(
            "reconstruct requires a ChunkStore; use FileCache::open(), not open_in_memory()",
        );

        let mut result = Vec::new();
        let mut bad_chunks = Vec::new();

        let total_size: usize = chunks.iter().map(|c| c.length as usize).sum();
        result.reserve(total_size);

        for chunk in chunks {
            match store.read_chunk(&chunk.hash).await {
                Ok(Some(data)) => {
                    let computed = Blake3Hash::new(&data);
                    if *computed.as_bytes() != chunk.hash {
                        tracing::warn!(
                            "cached chunk hash mismatch: expected {:?}, got {:?}",
                            &chunk.hash[..4],
                            &computed.as_bytes()[..4]
                        );
                        bad_chunks.push(chunk.hash);
                    } else if data.len() != chunk.length as usize {
                        tracing::warn!(
                            "cached chunk length mismatch: expected {}, got {}",
                            chunk.length,
                            data.len()
                        );
                        bad_chunks.push(chunk.hash);
                    } else {
                        result.extend_from_slice(&data);
                    }
                }
                Ok(None) => bad_chunks.push(chunk.hash),
                Err(e) => {
                    tracing::warn!("error reading chunk: {}", e);
                    bad_chunks.push(chunk.hash);
                }
            }
        }

        if bad_chunks.is_empty() {
            Ok(result)
        } else {
            Err(bad_chunks)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handle(byte: u8) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[0] = byte;
        Uuid::from_bytes(bytes)
    }

    #[tokio::test]
    async fn store_and_retrieve_root_hash() {
        let cache = FileCache::open_in_memory().await.unwrap();
        let handle = make_handle(1);

        let result = cache.get_root_hash(&handle).await.unwrap();
        assert!(result.is_none());

        let root = Blake3Hash::new(b"test-root");
        cache.put_root_hash(&handle, &root).await.unwrap();

        let result = cache.get_root_hash(&handle).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), root);
    }

    #[tokio::test]
    async fn store_and_retrieve_manifest() {
        let cache = FileCache::open_in_memory().await.unwrap();
        let handle = make_handle(1);

        let manifest = Manifest {
            root: Blake3Hash::new(b"test-root"),
            chunks: vec![
                ChunkInfo {
                    index: 0,
                    offset: 0,
                    length: 100,
                    hash: [0x01u8; 32],
                },
                ChunkInfo {
                    index: 1,
                    offset: 100,
                    length: 200,
                    hash: [0x02u8; 32],
                },
            ],
        };

        cache.put_manifest(&handle, &manifest).await.unwrap();

        let result = cache.get_manifest(&handle).await.unwrap();
        assert!(result.is_some());

        let retrieved = result.unwrap();
        assert_eq!(retrieved.root, manifest.root);
        assert_eq!(retrieved.chunks.len(), 2);
        assert_eq!(retrieved.chunks[0].hash, manifest.chunks[0].hash);
        assert_eq!(retrieved.chunks[1].hash, manifest.chunks[1].hash);
    }

    #[tokio::test]
    async fn reconstruct_all_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FileCache::open(tmp.path()).await.unwrap();

        let chunk0_data = b"hello ";
        let chunk1_data = b"world";
        let chunk0_hash = Blake3Hash::new(chunk0_data);
        let chunk1_hash = Blake3Hash::new(chunk1_data);

        cache
            .put_chunk(chunk0_hash.as_bytes(), chunk0_data)
            .await
            .unwrap();
        cache
            .put_chunk(chunk1_hash.as_bytes(), chunk1_data)
            .await
            .unwrap();

        let chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 6,
                hash: *chunk0_hash.as_bytes(),
            },
            ChunkInfo {
                index: 1,
                offset: 6,
                length: 5,
                hash: *chunk1_hash.as_bytes(),
            },
        ];

        let result = cache.reconstruct(&chunks).await.unwrap();
        assert_eq!(result, b"hello world");
    }

    #[tokio::test]
    async fn reconstruct_partial_cache_returns_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FileCache::open(tmp.path()).await.unwrap();

        let chunk0_data = b"hello ";
        let chunk1_data = b"world";
        let chunk0_hash = Blake3Hash::new(chunk0_data);
        let chunk1_hash = Blake3Hash::new(chunk1_data);

        cache
            .put_chunk(chunk0_hash.as_bytes(), chunk0_data)
            .await
            .unwrap();

        let chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 6,
                hash: *chunk0_hash.as_bytes(),
            },
            ChunkInfo {
                index: 1,
                offset: 6,
                length: 5,
                hash: *chunk1_hash.as_bytes(),
            },
        ];

        let result = cache.reconstruct(&chunks).await;
        assert!(result.is_err());
        let missing = result.unwrap_err();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], *chunk1_hash.as_bytes());
    }

    #[tokio::test]
    async fn reconstruct_detects_corrupted_chunk() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FileCache::open(tmp.path()).await.unwrap();

        let chunk0_data = b"hello ";
        let chunk0_hash = Blake3Hash::new(chunk0_data);

        // Write valid chunk data first
        cache
            .put_chunk(chunk0_hash.as_bytes(), chunk0_data)
            .await
            .unwrap();

        // Corrupt the file on disk by overwriting it with different data
        let path = {
            // Compute the file path the same way ChunkStore does
            let hex = {
                const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";
                let mut s = String::with_capacity(64);
                for &b in chunk0_hash.as_bytes() {
                    s.push(HEX_CHARS[(b >> 4) as usize] as char);
                    s.push(HEX_CHARS[(b & 0x0f) as usize] as char);
                }
                s
            };
            tmp.path()
                .join("chunks")
                .join(&hex[..2])
                .join(&hex[2..4])
                .join(format!("{hex}.bin"))
        };
        // Overwrite the file with garbage data
        std::fs::write(&path, b"CORRUPTED").unwrap();

        let chunks = vec![ChunkInfo {
            index: 0,
            offset: 0,
            length: 6,
            hash: *chunk0_hash.as_bytes(),
        }];

        let result = cache.reconstruct(&chunks).await;
        assert!(result.is_err(), "corrupted chunk data must be rejected");
        let bad_hashes = result.unwrap_err();
        assert_eq!(bad_hashes.len(), 1);
        // The returned hash should be the EXPECTED hash (chunk0_hash), not the
        // hash of the corrupted data
        assert_eq!(bad_hashes[0], *chunk0_hash.as_bytes());
    }

    #[tokio::test]
    async fn put_manifest_accumulates_chunks() {
        let cache = FileCache::open_in_memory().await.unwrap();
        let handle = make_handle(1);

        // First call: chunks 0 and 1
        let manifest1 = Manifest {
            root: Blake3Hash::new(b"test-root"),
            chunks: vec![
                ChunkInfo {
                    index: 0,
                    offset: 0,
                    length: 100,
                    hash: [0x01u8; 32],
                },
                ChunkInfo {
                    index: 1,
                    offset: 100,
                    length: 200,
                    hash: [0x02u8; 32],
                },
            ],
        };
        cache.put_manifest(&handle, &manifest1).await.unwrap();

        // Second call: chunks 2 and 3
        let manifest2 = Manifest {
            root: Blake3Hash::new(b"test-root"),
            chunks: vec![
                ChunkInfo {
                    index: 2,
                    offset: 300,
                    length: 300,
                    hash: [0x03u8; 32],
                },
                ChunkInfo {
                    index: 3,
                    offset: 600,
                    length: 400,
                    hash: [0x04u8; 32],
                },
            ],
        };
        cache.put_manifest(&handle, &manifest2).await.unwrap();

        // After both calls, the manifest should have ALL FOUR chunks
        let result = cache.get_manifest(&handle).await.unwrap().unwrap();
        assert_eq!(
            result.chunks.len(),
            4,
            "expected 4 accumulated chunks, got {}",
            result.chunks.len()
        );
        assert_eq!(result.chunks[0].index, 0);
        assert_eq!(result.chunks[1].index, 1);
        assert_eq!(result.chunks[2].index, 2);
        assert_eq!(result.chunks[3].index, 3);
    }

    #[tokio::test]
    async fn put_manifest_idempotent() {
        let cache = FileCache::open_in_memory().await.unwrap();
        let handle = make_handle(1);

        let manifest = Manifest {
            root: Blake3Hash::new(b"test-root"),
            chunks: vec![
                ChunkInfo {
                    index: 0,
                    offset: 0,
                    length: 100,
                    hash: [0xAAu8; 32],
                },
                ChunkInfo {
                    index: 1,
                    offset: 100,
                    length: 200,
                    hash: [0xBBu8; 32],
                },
            ],
        };

        // Call put_manifest twice with the same data
        cache.put_manifest(&handle, &manifest).await.unwrap();
        cache.put_manifest(&handle, &manifest).await.unwrap();

        // Should return exactly the same chunks — no duplicates, no errors
        let result = cache.get_manifest(&handle).await.unwrap().unwrap();
        assert_eq!(
            result.chunks.len(),
            2,
            "expected 2 chunks after idempotent put, got {}",
            result.chunks.len()
        );
        assert_eq!(result.chunks[0].index, 0);
        assert_eq!(result.chunks[0].hash, [0xAAu8; 32]);
        assert_eq!(result.chunks[1].index, 1);
        assert_eq!(result.chunks[1].hash, [0xBBu8; 32]);
    }

    #[tokio::test]
    async fn put_manifest_updates_stale_chunk_refs() {
        let cache = FileCache::open_in_memory().await.unwrap();
        let handle = make_handle(1);

        // First call: chunk 0 with hash A
        let manifest1 = Manifest {
            root: Blake3Hash::new(b"test-root"),
            chunks: vec![ChunkInfo {
                index: 0,
                offset: 0,
                length: 100,
                hash: [0xAAu8; 32],
            }],
        };
        cache.put_manifest(&handle, &manifest1).await.unwrap();

        // Second call: chunk 0 with hash B (updated)
        let manifest2 = Manifest {
            root: Blake3Hash::new(b"test-root"),
            chunks: vec![ChunkInfo {
                index: 0,
                offset: 0,
                length: 100,
                hash: [0xBBu8; 32],
            }],
        };
        cache.put_manifest(&handle, &manifest2).await.unwrap();

        // The newer value (hash B) should win
        let result = cache.get_manifest(&handle).await.unwrap().unwrap();
        assert_eq!(result.chunks.len(), 1);
        assert_eq!(result.chunks[0].index, 0);
        assert_eq!(
            result.chunks[0].hash, [0xBBu8; 32],
            "expected updated hash B for chunk 0"
        );
    }

    #[tokio::test]
    async fn cache_persists_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");

        let handle = make_handle(42);
        let root = Blake3Hash::new(b"persistent-root");
        let chunk_data = b"persistent chunk data";
        let chunk_hash = *Blake3Hash::new(chunk_data).as_bytes();

        {
            let cache = FileCache::open(&cache_dir).await.unwrap();
            cache.put_root_hash(&handle, &root).await.unwrap();
            cache
                .put_manifest(
                    &handle,
                    &Manifest {
                        root: root.clone(),
                        chunks: vec![ChunkInfo {
                            index: 0,
                            offset: 0,
                            length: chunk_data.len() as u64,
                            hash: chunk_hash,
                        }],
                    },
                )
                .await
                .unwrap();
            cache.put_chunk(&chunk_hash, chunk_data).await.unwrap();
        }

        let cache2 = FileCache::open(&cache_dir).await.unwrap();

        let loaded_root = cache2.get_root_hash(&handle).await.unwrap();
        assert_eq!(loaded_root, Some(root));

        let loaded_manifest = cache2.get_manifest(&handle).await.unwrap().unwrap();
        assert_eq!(loaded_manifest.chunks.len(), 1);
        assert_eq!(loaded_manifest.chunks[0].hash, chunk_hash);

        let loaded_chunk = cache2.get_chunk(&chunk_hash).await.unwrap();
        assert_eq!(loaded_chunk, Some(chunk_data.to_vec()));
    }
}
