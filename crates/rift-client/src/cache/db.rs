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
use rusqlite::{params, Connection, Result as SqliteResult};
use std::path::Path;
use std::sync::Mutex;

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
    conn: Mutex<Connection>,
}

impl FileCache {
    /// Open a cache at the given directory path.
    ///
    /// Creates the directory and database if they don't exist.
    pub fn open(cache_dir: &Path) -> SqliteResult<Self> {
        std::fs::create_dir_all(cache_dir).ok();
        let db_path = cache_dir.join("cache.db");
        let conn = Connection::open(db_path)?;

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
        )?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> SqliteResult<Self> {
        let conn = Connection::open_in_memory()?;
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
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Store the root hash for a file handle.
    pub fn put_root_hash(&self, handle: &[u8], root_hash: &Blake3Hash) -> SqliteResult<()> {
        let conn = self.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        conn.execute(
            "INSERT OR REPLACE INTO manifests (handle, root_hash, updated_at) VALUES (?1, ?2, ?3)",
            params![handle, root_hash.as_bytes(), now],
        )?;
        Ok(())
    }

    /// Get the cached root hash for a file handle.
    pub fn get_root_hash(&self, handle: &[u8]) -> SqliteResult<Option<Blake3Hash>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT root_hash FROM manifests WHERE handle = ?1",
            params![handle],
            |row| row.get::<_, Vec<u8>>(0),
        );

        match result {
            Ok(bytes) => Ok(Blake3Hash::from_slice(&bytes).ok()),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Store a file manifest (root hash + chunk list).
    pub fn put_manifest(&self, handle: &[u8], manifest: &Manifest) -> SqliteResult<()> {
        let conn = self.conn.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Upsert manifest
        conn.execute(
            "INSERT OR REPLACE INTO manifests (handle, root_hash, updated_at) VALUES (?1, ?2, ?3)",
            params![handle, manifest.root.as_bytes(), now],
        )?;

        // Delete old chunk refs
        conn.execute("DELETE FROM chunk_refs WHERE handle = ?1", params![handle])?;

        // Insert new chunk refs
        for chunk in &manifest.chunks {
            conn.execute(
                "INSERT INTO chunk_refs (handle, chunk_index, byte_offset, byte_length, chunk_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    handle,
                    chunk.index as i64,
                    chunk.offset as i64,
                    chunk.length as i64,
                    &chunk.hash[..]
                ],
            )?;
        }

        Ok(())
    }

    /// Get the cached manifest for a file handle.
    pub fn get_manifest(&self, handle: &[u8]) -> SqliteResult<Option<Manifest>> {
        let conn = self.conn.lock().unwrap();

        // Get root hash
        let root_bytes: Option<Vec<u8>> = match conn.query_row(
            "SELECT root_hash FROM manifests WHERE handle = ?1",
            params![handle],
            |row| row.get(0),
        ) {
            Ok(r) => Some(r),
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e),
        };

        let root = match root_bytes {
            Some(bytes) => {
                Blake3Hash::from_slice(&bytes).map_err(|_| rusqlite::Error::InvalidQuery)?
            }
            None => return Ok(None),
        };

        // Get chunk refs
        let mut stmt = conn.prepare(
            "SELECT chunk_index, byte_offset, byte_length, chunk_hash
             FROM chunk_refs WHERE handle = ?1 ORDER BY chunk_index",
        )?;

        let chunks: Vec<ChunkInfo> = stmt
            .query_map(params![handle], |row| {
                let hash_bytes: Vec<u8> = row.get(3)?;
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&hash_bytes);
                Ok(ChunkInfo {
                    index: row.get::<_, i64>(0)? as u32,
                    offset: row.get::<_, i64>(1)? as u64,
                    length: row.get::<_, i64>(2)? as u64,
                    hash,
                })
            })?
            .collect::<SqliteResult<Vec<_>>>()?;

        Ok(Some(Manifest { root, chunks }))
    }

    /// Store chunk data keyed by its hash.
    pub fn put_chunk(&self, hash: &[u8; 32], data: &[u8]) -> SqliteResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO chunk_data (chunk_hash, data) VALUES (?1, ?2)",
            params![hash, data],
        )?;
        Ok(())
    }

    /// Get chunk data by its hash.
    pub fn get_chunk(&self, hash: &[u8; 32]) -> SqliteResult<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT data FROM chunk_data WHERE chunk_hash = ?1",
            params![hash],
            |row| row.get(0),
        );

        match result {
            Ok(data) => Ok(Some(data)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Find chunks that are needed but not cached.
    pub fn missing_chunks(
        &self,
        _handle: &[u8],
        server_chunks: &[ChunkInfo],
    ) -> SqliteResult<Vec<ChunkInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut missing = Vec::new();

        for chunk in server_chunks {
            // Check if we have this chunk's data
            let has_chunk: bool = conn
                .query_row(
                    "SELECT 1 FROM chunk_data WHERE chunk_hash = ?1",
                    params![&chunk.hash[..]],
                    |_row| Ok(true),
                )
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
    pub fn reconstruct(&self, chunks: &[ChunkInfo]) -> Result<Vec<u8>, Vec<[u8; 32]>> {
        let mut result = Vec::new();
        let mut missing = Vec::new();

        // Pre-allocate based on chunk lengths
        let total_size: usize = chunks.iter().map(|c| c.length as usize).sum();
        result.reserve(total_size);

        for chunk in chunks {
            match self.get_chunk(&chunk.hash) {
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

    fn make_handle(name: &str) -> Vec<u8> {
        name.as_bytes().to_vec()
    }

    fn make_hash(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn store_and_retrieve_root_hash() {
        let cache = FileCache::open_in_memory().unwrap();
        let handle = make_handle("file1");

        // Initially empty
        let result = cache.get_root_hash(&handle).unwrap();
        assert!(result.is_none());

        // Store
        let root = Blake3Hash::new(b"test-root");
        cache.put_root_hash(&handle, &root).unwrap();

        // Retrieve
        let result = cache.get_root_hash(&handle).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), root);
    }

    #[test]
    fn store_and_retrieve_manifest() {
        let cache = FileCache::open_in_memory().unwrap();
        let handle = make_handle("file1");

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

        cache.put_manifest(&handle, &manifest).unwrap();

        let result = cache.get_manifest(&handle).unwrap();
        assert!(result.is_some());

        let retrieved = result.unwrap();
        assert_eq!(retrieved.root, manifest.root);
        assert_eq!(retrieved.chunks.len(), 2);
        assert_eq!(retrieved.chunks[0].hash, manifest.chunks[0].hash);
        assert_eq!(retrieved.chunks[1].hash, manifest.chunks[1].hash);
    }

    #[test]
    fn store_and_retrieve_chunk_data() {
        let cache = FileCache::open_in_memory().unwrap();
        let hash = make_hash(0xAB);
        let data = b"hello world".to_vec();

        // Initially empty
        let result = cache.get_chunk(&hash).unwrap();
        assert!(result.is_none());

        // Store
        cache.put_chunk(&hash, &data).unwrap();

        // Retrieve
        let result = cache.get_chunk(&hash).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), data);
    }

    #[test]
    fn missing_chunks_empty_cache() {
        let cache = FileCache::open_in_memory().unwrap();
        let _handle = make_handle("file1");

        let server_chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 100,
                hash: make_hash(0x01),
            },
            ChunkInfo {
                index: 1,
                offset: 100,
                length: 200,
                hash: make_hash(0x02),
            },
        ];

        let missing = cache.missing_chunks(&_handle, &server_chunks).unwrap();

        // All chunks should be missing (cache is empty)
        assert_eq!(missing.len(), 2);
    }

    #[test]
    fn missing_chunks_partial_cache() {
        let cache = FileCache::open_in_memory().unwrap();
        let handle = make_handle("file1");

        // Store one chunk
        cache.put_chunk(&make_hash(0x01), b"chunk1 data").unwrap();

        let server_chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 100,
                hash: make_hash(0x01),
            },
            ChunkInfo {
                index: 1,
                offset: 100,
                length: 200,
                hash: make_hash(0x02),
            },
        ];

        let missing = cache.missing_chunks(&handle, &server_chunks).unwrap();

        // Only chunk 2 should be missing
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].hash, make_hash(0x02));
    }

    #[test]
    fn missing_chunks_all_cached() {
        let cache = FileCache::open_in_memory().unwrap();
        let handle = make_handle("file1");

        // Store all chunks
        cache.put_chunk(&make_hash(0x01), b"chunk1").unwrap();
        cache.put_chunk(&make_hash(0x02), b"chunk2").unwrap();

        let server_chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 6,
                hash: make_hash(0x01),
            },
            ChunkInfo {
                index: 1,
                offset: 6,
                length: 6,
                hash: make_hash(0x02),
            },
        ];

        let missing = cache.missing_chunks(&handle, &server_chunks).unwrap();

        // No chunks missing
        assert!(missing.is_empty());
    }

    #[test]
    fn reconstruct_success() {
        let cache = FileCache::open_in_memory().unwrap();

        // Store all chunks
        cache.put_chunk(&make_hash(0x01), b"hello").unwrap();
        cache.put_chunk(&make_hash(0x02), b"world").unwrap();

        let chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 5,
                hash: make_hash(0x01),
            },
            ChunkInfo {
                index: 1,
                offset: 5,
                length: 5,
                hash: make_hash(0x02),
            },
        ];

        let result = cache.reconstruct(&chunks);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), b"helloworld".to_vec());
    }

    #[test]
    fn reconstruct_missing_chunks() {
        let cache = FileCache::open_in_memory().unwrap();

        // Store only the first chunk
        cache.put_chunk(&make_hash(0x01), b"hello").unwrap();

        let chunks = vec![
            ChunkInfo {
                index: 0,
                offset: 0,
                length: 5,
                hash: make_hash(0x01),
            },
            ChunkInfo {
                index: 1,
                offset: 5,
                length: 5,
                hash: make_hash(0x02),
            },
        ];

        let result = cache.reconstruct(&chunks);
        assert!(result.is_err());

        let missing = result.unwrap_err();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], make_hash(0x02));
    }
}
