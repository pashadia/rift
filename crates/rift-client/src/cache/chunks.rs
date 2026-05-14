//! File-based chunk storage module.
//!
//! Stores chunk data (binary blobs addressed by BLAKE3 hash) as individual files
//! on disk with directory sharding, replacing the previous `SQLite` BLOB storage.

use bytes::Bytes;
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
#[derive(Debug)]
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

        // Clean up temp files from previous crashes
        cleanup_temp_files(&chunks_dir).await;

        Ok(Self {
            base_dir: base_dir.to_path_buf(),
        })
    }

    /// Write a chunk to disk. Uses atomic write (write to temp file, then rename).
    ///
    /// If a chunk with the same hash already exists, this is idempotent
    /// (the data is identical).
    pub async fn write_chunk(&self, hash: &[u8; 32], data: &[u8]) -> io::Result<()> {
        // Write to a temp file first, then rename (atomic on same filesystem)
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = hash_to_path(&self.base_dir, hash);

        // Fast path: if chunk already exists with correct size, skip rewrite
        if let Ok(metadata) = tokio::fs::metadata(&path).await {
            if metadata.len() == data.len() as u64 {
                // File exists with correct size — content hash is verified by path
                return Ok(());
            }
            // File exists but wrong size — could be corruption or hash collision.
            // Proceed with rewrite (size mismatch is suspicious).
        }

        // Write to a temp file first, then rename (atomic on same filesystem)
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hash path has no parent"))?;

        // Create shard directories if needed
        tokio::fs::create_dir_all(parent).await?;

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
    pub async fn read_chunk(&self, hash: &[u8; 32]) -> io::Result<Option<Bytes>> {
        let path = hash_to_path(&self.base_dir, hash);
        match tokio::fs::read(&path).await {
            Ok(data) => Ok(Some(Bytes::from(data))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Check if a chunk exists on disk.
    pub async fn chunk_exists(&self, hash: &[u8; 32]) -> io::Result<bool> {
        let path = hash_to_path(&self.base_dir, hash);
        Ok(tokio::fs::metadata(&path).await.is_ok())
    }

    /// Write a chunk from `Bytes`. Thin wrapper around [`write_chunk`].
    pub async fn write_chunk_from_bytes(&self, hash: &[u8; 32], data: Bytes) -> io::Result<()> {
        self.write_chunk(hash, &data).await
    }

    /// Read a byte range from a chunk on disk.
    ///
    /// Uses `tokio::fs::File` + `seek` + `read_exact` for async partial reads,
    /// avoiding the full-file read of [`read_chunk`].
    /// Returns `None` if the chunk doesn't exist, or `Err` if the range
    /// extends past the end of the stored file.
    pub async fn read_chunk_range(
        &self,
        hash: &[u8; 32],
        range_offset: u64,
        len: usize,
    ) -> io::Result<Option<Bytes>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let path = hash_to_path(&self.base_dir, hash);

        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };

        file.seek(std::io::SeekFrom::Start(range_offset)).await?;

        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).await?;

        Ok(Some(Bytes::from(buf)))
    }

    /// Verify a stored chunk's file size matches `expected_len`.
    ///
    /// Returns `Ok(())` if the file exists with the correct size,
    /// or an `io::Error` if the file is missing, truncated, or extended.
    pub async fn verify_chunk_size(&self, hash: &[u8; 32], expected_len: u64) -> io::Result<()> {
        let path = hash_to_path(&self.base_dir, hash);
        let metadata = tokio::fs::metadata(&path).await?;
        if metadata.len() != expected_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "chunk size mismatch: expected {}, got {}",
                    expected_len,
                    metadata.len()
                ),
            ));
        }
        Ok(())
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

/// Clean up leftover temporary files (`.tmp_*`) from previous crashes.
///
/// Recursively walks the chunks directory, scanning each shard subdirectory
/// for files starting with `.tmp_` and deleting them.
async fn cleanup_temp_files(chunks_dir: &Path) {
    let Ok(mut top_entries) = tokio::fs::read_dir(chunks_dir).await else {
        return;
    };
    while let Ok(Some(top_entry)) = top_entries.next_entry().await {
        if !top_entry
            .file_type()
            .await
            .map(|t| t.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let shard_dir1 = top_entry.path();
        let Ok(mut shard_entries) = tokio::fs::read_dir(&shard_dir1).await else {
            continue;
        };
        while let Ok(Some(shard_entry)) = shard_entries.next_entry().await {
            if !shard_entry
                .file_type()
                .await
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let shard_dir2 = shard_entry.path();
            let Ok(mut file_entries) = tokio::fs::read_dir(&shard_dir2).await else {
                continue;
            };
            while let Ok(Some(file_entry)) = file_entries.next_entry().await {
                let file_name = file_entry.file_name();
                let name = file_name.to_string_lossy();
                if name.starts_with(".tmp_") {
                    let _ = tokio::fs::remove_file(file_entry.path()).await;
                }
            }
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
        assert_eq!(result.as_ref().map(|b| &b[..]), Some(&data[..]));
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
        assert_eq!(result.as_ref().map(|b| &b[..]), Some(&data[..]));
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
        assert_eq!(result1.as_ref().map(|b| &b[..]), Some(&data1[..]));
        assert_eq!(result2.as_ref().map(|b| &b[..]), Some(&data2[..]));
    }

    #[tokio::test]
    async fn cleanup_temp_files_on_open() {
        // Test issue #1: ChunkStore::open should clean up leftover .tmp_* files
        let tmp = TempDir::new().unwrap();
        let base_dir = tmp.path();

        // Create a ChunkStore and write a chunk to populate shard directories
        let store = ChunkStore::open(base_dir).await.unwrap();
        let hash = make_hash(0xAA);
        store.write_chunk(&hash, b"some data").await.unwrap();
        drop(store);

        // Manually create a leftover .tmp_ file in the shard directory
        // (simulating a crash between write and rename)
        let shard_dir = base_dir.join("chunks").join("aa").join("aa");
        let tmp_file = shard_dir.join(".tmp_test_crash_leftover");
        tokio::fs::write(&tmp_file, b"stale temp data")
            .await
            .unwrap();
        assert!(tmp_file.exists(), "temp file should exist before cleanup");

        // Open a new ChunkStore — should clean up temp files
        let _store2 = ChunkStore::open(base_dir).await.unwrap();

        // The .tmp_ file should be gone
        assert!(
            !tmp_file.exists(),
            "leftover temp file should be cleaned up on open"
        );
    }

    #[tokio::test]
    async fn write_chunk_skips_identical_data() {
        // Test issue #2: write_chunk should skip rewriting identical data
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let hash = make_hash(0xBB);
        let data = b"hello";

        // Write chunk once
        store.write_chunk(&hash, data).await.unwrap();

        // Count files in the shard directory before second write
        let path = hash_to_path(tmp.path(), &hash);
        let shard_dir = path.parent().unwrap();
        let files_before: Vec<_> = std::fs::read_dir(shard_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();

        // Write same chunk again — should be a no-op (skip rewrite)
        store.write_chunk(&hash, data).await.unwrap();

        // Count files after second write
        let files_after: Vec<_> = std::fs::read_dir(shard_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();

        // Should have exactly the same number of files (no temp files left)
        assert_eq!(
            files_before.len(),
            files_after.len(),
            "second write of identical data should not create additional files"
        );

        // Content should still be correct
        let result = store.read_chunk(&hash).await.unwrap();
        assert_eq!(result.as_ref().map(|b| &b[..]), Some(&data[..]));
    }

    #[tokio::test]
    async fn write_chunk_rewrites_on_size_mismatch() {
        // Test issue #2: write_chunk should rewrite when file size doesn't match
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let hash = make_hash(0xCC);
        let original_data = b"short";

        // Write chunk
        store.write_chunk(&hash, original_data).await.unwrap();

        // Manually truncate the file to corrupt it (different size)
        let path = hash_to_path(tmp.path(), &hash);
        std::fs::write(&path, b"x").unwrap(); // wrong size + wrong content

        // Write again with correct but different-length data — should rewrite
        let new_data = b"longer replacement data";
        store.write_chunk(&hash, new_data).await.unwrap();

        // Verify file was rewritten with correct content
        let result = store.read_chunk(&hash).await.unwrap();
        assert_eq!(result.as_ref().map(|b| &b[..]), Some(&new_data[..]));
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

    // ── write_chunk_from_bytes tests ──────────────────────────────────

    #[tokio::test]
    async fn write_chunk_from_bytes_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let hash = make_hash(0xFE);
        let data = Bytes::from_static(b"bytes-based chunk data!");

        store
            .write_chunk_from_bytes(&hash, data.clone())
            .await
            .unwrap();
        let result = store.read_chunk(&hash).await.unwrap();
        assert_eq!(result.as_ref().map(|b| &b[..]), Some(&data[..]));
    }

    #[tokio::test]
    async fn write_chunk_from_bytes_idempotent() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let hash = make_hash(0xFD);
        let data = Bytes::from_static(b"idempotent bytes test");

        store
            .write_chunk_from_bytes(&hash, data.clone())
            .await
            .unwrap();
        store
            .write_chunk_from_bytes(&hash, data.clone())
            .await
            .unwrap();

        let result = store.read_chunk(&hash).await.unwrap();
        assert_eq!(result.as_ref().map(|b| &b[..]), Some(&data[..]));
    }

    #[tokio::test]
    async fn write_chunk_from_bytes_vs_slice_equivalence() {
        let tmp = TempDir::new().unwrap();
        let store1 = ChunkStore::open(tmp.path()).await.unwrap();

        let data = b"same data, different wrappers";
        let bytes = Bytes::from_static(data);
        let hash = make_hash(0xFC);

        // Write via bytes path
        store1.write_chunk_from_bytes(&hash, bytes).await.unwrap();
        let result1 = store1.read_chunk(&hash).await.unwrap();

        // Delete and re-write via slice path
        store1.delete_chunk(&hash).await.unwrap();
        store1.write_chunk(&hash, data).await.unwrap();
        let result2 = store1.read_chunk(&hash).await.unwrap();

        // Both paths should produce identical on-disk data
        assert_eq!(
            result1.as_ref().map(|b| &b[..]),
            result2.as_ref().map(|b| &b[..])
        );
        assert_eq!(result1.as_ref().map(|b| &b[..]), Some(&data[..]));
    }

    // ── read_chunk_range tests ───────────────────────────────────────

    #[tokio::test]
    async fn read_chunk_range_middle_of_chunk() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let data: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let hash = make_hash(0xE0);
        store.write_chunk(&hash, &data).await.unwrap();

        let result = store
            .read_chunk_range(&hash, 1024, 1024)
            .await
            .unwrap()
            .expect("chunk exists");
        assert_eq!(result, &data[1024..2048]);
    }

    #[tokio::test]
    async fn read_chunk_range_from_offset_zero() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let data = b"hello from the beginning";
        let hash = make_hash(0xE1);
        store.write_chunk(&hash, data).await.unwrap();

        let result = store
            .read_chunk_range(&hash, 0, 5)
            .await
            .unwrap()
            .expect("chunk exists");
        assert_eq!(&result[..], b"hello");
    }

    #[tokio::test]
    async fn read_chunk_range_past_end_is_error() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let data = b"short";
        let hash = make_hash(0xE2);
        store.write_chunk(&hash, data).await.unwrap();

        let result = store.read_chunk_range(&hash, 3, 100).await;
        assert!(
            result.is_err(),
            "read past end should error, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn read_chunk_range_offset_past_file_is_error() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let data = b"tiny";
        let hash = make_hash(0xE3);
        store.write_chunk(&hash, data).await.unwrap();

        let result = store.read_chunk_range(&hash, 100, 10).await;
        assert!(
            result.is_err(),
            "offset past end should error, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn read_chunk_range_nonexistent_returns_none() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let hash = make_hash(0xE4);
        let result = store.read_chunk_range(&hash, 0, 100).await.unwrap();
        assert!(result.is_none(), "non-existent chunk should return None");
    }

    #[tokio::test]
    async fn read_chunk_range_full_chunk_matches_read_chunk() {
        let tmp = TempDir::new().unwrap();
        let store = ChunkStore::open(tmp.path()).await.unwrap();

        let data: Vec<u8> = (b'A'..=b'Z').cycle().take(1000).collect();
        let hash = make_hash(0xE5);
        store.write_chunk(&hash, &data).await.unwrap();

        let full_range = store
            .read_chunk_range(&hash, 0, data.len())
            .await
            .unwrap()
            .expect("chunk exists");
        let full = store
            .read_chunk(&hash)
            .await
            .unwrap()
            .expect("chunk exists");
        assert_eq!(&full_range[..], &full[..]);
    }
}
