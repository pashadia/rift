//! SQLite-based file cache.
//!
//! Stores:
//! - File manifests: (handle -> root_hash, chunk list)
//! - Chunk data: (chunk_hash -> data) content-addressable storage
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
pub struct FileCache {
    conn: Connection,
}

impl FileCache {
    /// Open a cache at the given directory path.
    ///
    /// Creates the directory and database if they don't exist.
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

                 CREATE TABLE IF NOT EXISTS chunk_data (
                     chunk_hash  BLOB PRIMARY KEY,
                     data        BLOB NOT NULL
                 );

                 CREATE INDEX IF NOT EXISTS idx_chunk_refs_hash ON chunk_refs(chunk_hash);
                 CREATE INDEX IF NOT EXISTS idx_chunk_refs_handle ON chunk_refs(handle);",
            )
        })
        .await?;

        Ok(Self { conn })
    }

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

                 CREATE TABLE IF NOT EXISTS chunk_data (
                     chunk_hash  BLOB PRIMARY KEY,
                     data        BLOB NOT NULL
                 );

                 CREATE INDEX IF NOT EXISTS idx_chunk_refs_hash ON chunk_refs(chunk_hash);",
            )
        })
        .await?;

        Ok(Self { conn })
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

            tx.execute("DELETE FROM chunk_refs WHERE handle = ?1", [&handle_str])?;

            for (index, offset, length, hash) in &chunks {
                tx.execute(
                    "INSERT INTO chunk_refs (handle, chunk_index, byte_offset, byte_length, chunk_hash)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    (&handle_str, index, offset, length, hash),
                )?;
            }

            tx.commit()
        })
        .await?;
        Ok(())
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
    pub async fn put_chunk(&self, hash: &[u8; 32], data: &[u8]) -> SqliteResult<()> {
        let hash_vec = hash.to_vec();
        let data_vec = data.to_vec();

        self.call(move |conn: &mut rusqlite::Connection| {
            conn.execute(
                "INSERT OR REPLACE INTO chunk_data (chunk_hash, data) VALUES (?1, ?2)",
                (&hash_vec, &data_vec),
            )
        })
        .await?;
        Ok(())
    }

    /// Get chunk data by its hash.
    pub async fn get_chunk(&self, hash: &[u8; 32]) -> SqliteResult<Option<Vec<u8>>> {
        let hash_vec = hash.to_vec();

        let result = self
            .call(move |conn: &mut rusqlite::Connection| {
                conn.query_row(
                    "SELECT data FROM chunk_data WHERE chunk_hash = ?1",
                    [&hash_vec],
                    |row| row.get(0),
                )
            })
            .await;

        match result {
            Ok(data) => Ok(Some(data)),
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

    /// Find chunks that are needed but not cached.
    pub async fn missing_chunks(
        &self,
        _handle: &Uuid,
        server_chunks: &[ChunkInfo],
    ) -> SqliteResult<Vec<ChunkInfo>> {
        let mut missing = Vec::new();

        for chunk in server_chunks {
            let hash_vec = chunk.hash.to_vec();
            let has_chunk = self
                .call(move |conn: &mut rusqlite::Connection| {
                    let result: std::result::Result<i32, _> = conn.query_row(
                        "SELECT 1 FROM chunk_data WHERE chunk_hash = ?1",
                        [&hash_vec],
                        |row| row.get(0),
                    );
                    Ok(result.is_ok())
                })
                .await
                .unwrap_or(false);

            if !has_chunk {
                missing.push(chunk.clone());
            }
        }

        Ok(missing)
    }

    /// Reconstruct a file from cached chunks.
    ///
    /// Returns `Err` listing the hashes of missing chunks if any chunk data is unavailable.
    pub async fn reconstruct(&self, chunks: &[ChunkInfo]) -> Result<Vec<u8>, Vec<[u8; 32]>> {
        let mut result = Vec::new();
        let mut missing = Vec::new();

        let total_size: usize = chunks.iter().map(|c| c.length as usize).sum();
        result.reserve(total_size);

        for chunk in chunks {
            match self.get_chunk(&chunk.hash).await {
                Ok(Some(data)) => result.extend_from_slice(&data),
                Ok(None) => missing.push(chunk.hash),
                Err(e) => {
                    missing.push(chunk.hash);
                    tracing::warn!("error reading chunk: {}", e);
                }
            }
        }

        if missing.is_empty() {
            Ok(result)
        } else {
            Err(missing)
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

    fn make_hash(byte: u8) -> [u8; 32] {
        [byte; 32]
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
    async fn store_and_retrieve_chunk_data() {
        let cache = FileCache::open_in_memory().await.unwrap();
        let hash = make_hash(0xAB);
        let data = b"hello world".to_vec();

        let result = cache.get_chunk(&hash).await.unwrap();
        assert!(result.is_none());

        cache.put_chunk(&hash, &data).await.unwrap();

        let result = cache.get_chunk(&hash).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), data);
    }

    #[tokio::test]
    async fn missing_chunks_empty_cache() {
        let cache = FileCache::open_in_memory().await.unwrap();
        let handle = make_handle(1);

        let server_chunks = vec![
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
        ];

        let missing = cache.missing_chunks(&handle, &server_chunks).await.unwrap();
        assert_eq!(missing.len(), 2);
    }

    #[tokio::test]
    async fn missing_chunks_partial_cache() {
        let cache = FileCache::open_in_memory().await.unwrap();
        let handle = make_handle(1);

        cache
            .put_chunk(&[0x01u8; 32], b"chunk1 data")
            .await
            .unwrap();

        let server_chunks = vec![
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
        ];

        let missing = cache.missing_chunks(&handle, &server_chunks).await.unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].hash, [0x02u8; 32]);
    }

    #[tokio::test]
    async fn reconstruct_all_cached() {
        let cache = FileCache::open_in_memory().await.unwrap();

        cache.put_chunk(&[0x01u8; 32], b"hello ").await.unwrap();
        cache.put_chunk(&[0x02u8; 32], b"world").await.unwrap();

        let chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 6,
                hash: [0x01u8; 32],
            },
            ChunkInfo {
                index: 1,
                offset: 6,
                length: 5,
                hash: [0x02u8; 32],
            },
        ];

        let result = cache.reconstruct(&chunks).await.unwrap();
        assert_eq!(result, b"hello world");
    }

    #[tokio::test]
    async fn reconstruct_partial_cache_returns_missing() {
        let cache = FileCache::open_in_memory().await.unwrap();

        cache.put_chunk(&[0x01u8; 32], b"hello ").await.unwrap();

        let chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 6,
                hash: [0x01u8; 32],
            },
            ChunkInfo {
                index: 1,
                offset: 6,
                length: 5,
                hash: [0x02u8; 32],
            },
        ];

        let result = cache.reconstruct(&chunks).await;
        assert!(result.is_err());
        let missing = result.unwrap_err();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], [0x02u8; 32]);
    }

    #[tokio::test]
    async fn cache_persists_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");

        let handle = make_handle(42);
        let root = Blake3Hash::new(b"persistent-root");
        let chunk_hash = [0xABu8; 32];
        let chunk_data = b"persistent chunk data";

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
