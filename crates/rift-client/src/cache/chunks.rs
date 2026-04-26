//! File-based chunk storage module.
//!
//! Stores chunk data (binary blobs addressed by BLAKE3 hash) as individual files
//! on disk with directory sharding, replacing the previous SQLite BLOB storage.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A file-based content-addressable chunk store.
///
/// Chunks are stored under `base_dir/chunks/` with two levels of directory
/// sharding based on the hex-encoded BLAKE3 hash:
///
/// ```text
/// base_dir/chunks/ab/cd/abcd...0123.bin
/// ```
pub struct ChunkStore {
    base_dir: PathBuf,
}

impl ChunkStore {
    /// Open/create a chunk store at the given base directory.
    ///
    /// Creates the directory structure (`base_dir/chunks/`) if it doesn't exist.
    pub async fn open(base_dir: &Path) -> io::Result<Self> {
        let chunks_dir = base_dir.join("chunks");
        tokio::fs::create_dir_all(&chunks_dir).await?;
        Ok(Self {
            base_dir: base_dir.to_path_buf(),
        })
    }

    /// Write a chunk to disk. Uses atomic write (write to temp file, then rename).
    ///
    /// If a chunk with the same hash already exists, this is idempotent
    /// (the data is identical).
    pub async fn write_chunk(&self, hash: &[u8; 32], data: &[u8]) -> io::Result<()> {
        let path = hash_to_path(&self.base_dir, hash);
        let parent = path.parent().expect("hash path always has a parent");

        // Create shard directories if needed
        tokio::fs::create_dir_all(parent).await?;

        // Write to a temp file first, then rename (atomic on same filesystem)
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let tmp_name = format!(
            ".tmp_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let tmp_path = parent.join(&tmp_name);

        if let Err(e) = tokio::fs::write(&tmp_path, data).await {
            // Clean up temp file on error
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e);
        }

        // Rename is atomic on the same filesystem
        match tokio::fs::rename(&tmp_path, &path).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Try to clean up temp file on rename failure
                let _ = tokio::fs::remove_file(&tmp_path).await;
                Err(e)
            }
        }
    }

    /// Read a chunk from disk. Returns `None` if the chunk doesn't exist.
    pub async fn read_chunk(&self, hash: &[u8; 32]) -> io::Result<Option<Vec<u8>>> {
        let path = hash_to_path(&self.base_dir, hash);
        match tokio::fs::read(&path).await {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Check if a chunk exists on disk.
    pub async fn chunk_exists(&self, hash: &[u8; 32]) -> io::Result<bool> {
        let path = hash_to_path(&self.base_dir, hash);
        Ok(tokio::fs::metadata(&path).await.is_ok())
    }

    /// Delete a chunk from disk. No-op if the chunk doesn't exist.
    pub async fn delete_chunk(&self, hash: &[u8; 32]) -> io::Result<()> {
        let path = hash_to_path(&self.base_dir, hash);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Convert a 32-byte BLAKE3 hash to its shard file path.
///
/// The hex representation is split into directory shards:
/// `base_dir/chunks/{hex[0..2]}/{hex[2..4]}/{hex}.bin`
fn hash_to_path(base_dir: &Path, hash: &[u8; 32]) -> PathBuf {
    let hex = bytes_to_hex(hash);
    base_dir
        .join("chunks")
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(format!("{hex}.bin"))
}

/// Encode a byte slice as lowercase hex.
fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX_CHARS[(b >> 4) as usize] as char);
        s.push(HEX_CHARS[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_hash(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let hash = make_hash(0xAB);
        let data = b"hello, chunk storage!";

        store.write_chunk(&hash, data).await.unwrap();
        let result = store.read_chunk(&hash).await.unwrap();
        assert_eq!(result, Some(data.to_vec()));
    }

    #[tokio::test]
    async fn read_missing_returns_none() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let hash = make_hash(0x00);
        let result = store.read_chunk(&hash).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn chunk_exists_correct() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let hash = make_hash(0x42);
        let data = b"exists test";

        // Before write
        assert!(!store.chunk_exists(&hash).await.unwrap());

        store.write_chunk(&hash, data).await.unwrap();

        // After write
        assert!(store.chunk_exists(&hash).await.unwrap());

        store.delete_chunk(&hash).await.unwrap();

        // After delete
        assert!(!store.chunk_exists(&hash).await.unwrap());
    }

    #[test]
    fn hash_to_path_sharding() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        // hash starting with [0xAB, 0xCD, ...]
        let mut hash = [0u8; 32];
        hash[0] = 0xAB;
        hash[1] = 0xCD;

        let path = hash_to_path(base, &hash);
        let path_str = path.to_string_lossy();

        // Path should contain "ab/cd/" as shard directories
        assert!(
            path_str.contains("ab/cd/"),
            "expected path to contain 'ab/cd/', got: {}",
            path_str
        );

        // Path should end with .bin
        assert!(
            path_str.ends_with(".bin"),
            "expected path to end with '.bin', got: {}",
            path_str
        );
    }

    #[tokio::test]
    async fn write_chunk_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let hash = make_hash(0x11);
        let data = b"idempotent data";

        store.write_chunk(&hash, data).await.unwrap();
        store.write_chunk(&hash, data).await.unwrap();

        let result = store.read_chunk(&hash).await.unwrap();
        assert_eq!(result, Some(data.to_vec()));
    }

    #[tokio::test]
    async fn delete_nonexistent_is_ok() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let hash = make_hash(0xFF);
        // Deleting a hash that was never written should return Ok
        store.delete_chunk(&hash).await.unwrap();
    }

    #[tokio::test]
    async fn write_creates_directories() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let mut hash = [0u8; 32];
        hash[0] = 0xDE;
        hash[1] = 0xAD;

        store.write_chunk(&hash, b"dir test").await.unwrap();

        // Verify the sharded directories were created
        let shard_dir = tmp.path().join("chunks").join("de").join("ad");
        assert!(
            shard_dir.exists(),
            "shard directory should exist: {:?}",
            shard_dir
        );
    }

    #[tokio::test]
    async fn concurrent_write_chunk_no_collision() {
        // Two ChunkStore instances sharing the same base dir should be able
        // to write different chunks without colliding on temp files.
        // The PID + counter in the temp name ensures uniqueness across processes.
        let tmp = TempDir::new().unwrap();
        let store1 = ChunkStore::open(tmp.path()).await.unwrap();
        let store2 = ChunkStore::open(tmp.path()).await.unwrap();

        let hash1 = make_hash(0x01);
        let hash2 = make_hash(0x02);
        let data1 = b"data from store 1";
        let data2 = b"data from store 2";

        // Write from both stores
        store1.write_chunk(&hash1, data1).await.unwrap();
        store2.write_chunk(&hash2, data2).await.unwrap();

        // Both chunks should be readable and contain correct data
        let result1 = store1.read_chunk(&hash1).await.unwrap();
        let result2 = store2.read_chunk(&hash2).await.unwrap();
        assert_eq!(result1, Some(data1.to_vec()));
        assert_eq!(result2, Some(data2.to_vec()));
    }

    #[tokio::test]
    async fn open_creates_base_dir() {
        let tmp = TempDir::new().unwrap();
        let new_dir = tmp.path().join("new_cache_dir");

        assert!(!new_dir.exists());

        let _store = ChunkStore::open(&new_dir).await.unwrap();

        let chunks_dir = new_dir.join("chunks");
        assert!(
            chunks_dir.exists(),
            "chunks directory should be created: {:?}",
            chunks_dir
        );
    }
}
