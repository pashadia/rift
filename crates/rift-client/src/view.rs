//! Defines the `ShareView` trait, the filesystem-level abstraction that the FUSE
//! implementation uses.

use crate::cache::db::FileCache;
use crate::remote::RemoteShare;
use async_trait::async_trait;
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, ReaddirEntry};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::instrument;

/// The `ShareView` trait represents a simple, synchronous, byte-oriented view of a
/// filesystem. It is completely ignorant of the underlying network protocol,
/// chunks, or Merkle trees.
///
/// The FUSE implementation (`RiftFilesystem`) interacts exclusively with this trait.
#[async_trait]
pub trait ShareView: Send + Sync + 'static {
    /// Gets attributes for a single object.
    async fn getattr(&self, handle: &[u8]) -> Result<FileAttrs, FsError>;

    /// Looks up an entry in a directory.
    async fn lookup(&self, parent: &[u8], name: &str) -> Result<(Vec<u8>, FileAttrs), FsError>;

    /// Reads directory entries with their attributes.
    async fn readdirplus(&self, handle: &[u8]) -> Result<Vec<(ReaddirEntry, FileAttrs)>, FsError>;

    /// Reads a byte range from a file.
    ///
    /// This method handles all the complexity of Merkle tree-based reads:
    /// - Fetches file attributes (including Merkle root)
    /// - Validates against cached root hash if available
    /// - On mismatch: drills the Merkle tree to determine needed chunks
    /// - Fetches needed chunks from the server
    /// - Assembles chunks into a contiguous byte buffer
    /// - Returns the requested byte range [offset, offset + length)
    async fn read(
        &self,
        handle: &[u8],
        offset: u64,
        length: u64,
        cached_root_hash: Option<&[u8]>,
    ) -> Result<Vec<u8>, FsError>;
}

/// The `RiftShareView` is the primary implementation of the `ShareView` trait.
/// It acts as a "pass-through" adapter, translating the simple, synchronous
/// filesystem calls from the FUSE layer into the asynchronous, protocol-level
/// calls of the `RemoteShare` trait.
///
/// Optionally uses a `FileCache` for caching file manifests and chunk data.
pub struct RiftShareView<R: RemoteShare> {
    remote: Arc<R>,
    cache: Option<Arc<FileCache>>,
}

impl<R: RemoteShare> RiftShareView<R> {
    pub fn new(remote: Arc<R>) -> Self {
        Self {
            remote,
            cache: None,
        }
    }

    pub async fn with_cache(remote: Arc<R>, cache_dir: PathBuf) -> anyhow::Result<Self> {
        let cache = FileCache::open(&cache_dir).await?;
        Ok(Self {
            remote,
            cache: Some(Arc::new(cache)),
        })
    }
}

#[async_trait]
impl<R: RemoteShare> ShareView for RiftShareView<R> {
    #[instrument(skip(self), fields(handle_len = handle.len()))]
    async fn getattr(&self, handle: &[u8]) -> Result<FileAttrs, FsError> {
        self.remote
            .stat_batch(vec![handle.to_vec()])
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?
            .remove(0)
    }

    #[instrument(skip(self), fields(parent_len = parent.len(), name = %name))]
    async fn lookup(&self, parent: &[u8], name: &str) -> Result<(Vec<u8>, FileAttrs), FsError> {
        self.remote
            .lookup(parent, name)
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))
    }

    #[instrument(skip(self), fields(handle_len = handle.len()))]
    async fn readdirplus(&self, handle: &[u8]) -> Result<Vec<(ReaddirEntry, FileAttrs)>, FsError> {
        let entries = self
            .remote
            .readdir(handle)
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?;
        if entries.is_empty() {
            return Ok(vec![]);
        }

        let handles: Vec<Vec<u8>> = entries.iter().map(|e| e.handle.clone()).collect();
        let attrs_results = self
            .remote
            .stat_batch(handles)
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?;

        let combined: Vec<_> = entries
            .into_iter()
            .zip(attrs_results.into_iter())
            .filter_map(|(entry, attrs_result)| attrs_result.ok().map(|attrs| (entry, attrs)))
            .collect();

        Ok(combined)
    }

    #[instrument(skip(self), fields(handle_len = handle.len(), offset, length))]
    async fn read(
        &self,
        handle: &[u8],
        offset: u64,
        length: u64,
        _cached_root_hash: Option<&[u8]>,
    ) -> Result<Vec<u8>, FsError> {
        let attrs = self
            .remote
            .stat_batch(vec![handle.to_vec()])
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?
            .remove(0)?;

        let file_size = attrs.size;
        if attrs.root_hash.is_empty() {
            return Err(FsError::Io);
        }
        let merkle_root = attrs.root_hash;

        if file_size == 0 {
            return Ok(vec![]);
        }

        // Check cache for manifest and chunk data
        if let Some(ref cache) = self.cache {
            // Try to get manifest from cache
            match cache.get_manifest(handle).await {
                Ok(Some(manifest)) => {
                    // Check if root hash matches
                    if manifest.root.as_bytes() == merkle_root.as_slice() {
                        // Root matches - try to reconstruct from cache
                        match cache.reconstruct(&manifest.chunks).await {
                            Ok(data) => {
                                let start = offset as usize;
                                let end = (offset + length).min(file_size) as usize;
                                if end <= data.len() {
                                    tracing::debug!("read {} bytes from cache", end - start);
                                    return Ok(data[start..end].to_vec());
                                }
                            }
                            Err(missing_hashes) => {
                                tracing::debug!("cache miss for {} chunks", missing_hashes.len());
                                // Continue to fetch missing chunks
                            }
                        }
                    } else {
                        tracing::debug!("root hash changed, cache invalid");
                    }
                }
                Ok(None) => {
                    tracing::debug!("no cached manifest for handle");
                }
                Err(e) => {
                    tracing::warn!("cache error: {}", e);
                }
            }
        }

        // Cache miss or validation failed - fetch from server
        let end = (offset + length).min(file_size);
        let chunk_size = 128 * 1024u64;
        let start_chunk = (offset / chunk_size) as u32;
        let end_chunk = end.div_ceil(chunk_size) as u32;
        let chunk_count = end_chunk - start_chunk;

        let drill_result = self
            .remote
            .merkle_drill(handle, 1, &[])
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?;

        let chunk_hashes = drill_result.hashes;
        let mut needed_chunks = Vec::new();
        for (i, hash) in chunk_hashes.iter().enumerate() {
            if i >= start_chunk as usize && i < end_chunk as usize {
                needed_chunks.push((i as u32, hash.clone()));
            }
        }

        if needed_chunks.is_empty() {
            return Ok(vec![]);
        }

        let read_result = self
            .remote
            .read_chunks(handle, start_chunk, chunk_count)
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?;

        // Store fetched chunks in cache
        if let Some(ref cache) = self.cache {
            for chunk in &read_result.chunks {
                let _ = cache.put_chunk(&chunk.hash, &chunk.data).await;
            }
            // Store manifest for future cache hits
            let root = rift_common::crypto::Blake3Hash::from_slice(&merkle_root)
                .unwrap_or_else(|_| rift_common::crypto::Blake3Hash::from_array([0u8; 32]));
            let manifest = crate::cache::db::Manifest {
                root,
                chunks: read_result
                    .chunks
                    .iter()
                    .map(|c| crate::cache::db::ChunkInfo {
                        index: c.index,
                        offset: c.index as u64 * chunk_size,
                        length: c.length,
                        hash: c.hash,
                    })
                    .collect(),
            };
            let _ = cache.put_manifest(handle, &manifest).await;
        }

        let mut all_data = Vec::new();
        for chunk in read_result.chunks {
            all_data.extend(chunk.data);
        }

        let start_offset = (offset % chunk_size) as usize;
        let requested_length = (end - offset) as usize;
        let result = all_data
            .get(start_offset..start_offset + requested_length)
            .map(|s| s.to_vec())
            .unwrap_or_else(|| {
                all_data
                    .get(start_offset..)
                    .map(|s| s.to_vec())
                    .unwrap_or_default()
            });

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::db::{ChunkInfo, FileCache, Manifest};
    use crate::client::{ChunkData, ChunkReadResult, MerkleDrillResult};
    use async_trait::async_trait;
    use rift_common::crypto::Blake3Hash;
    use rift_protocol::messages::ReaddirEntry;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[allow(clippy::type_complexity)]
    struct MockRemote {
        lookup_result: Mutex<Option<anyhow::Result<(Vec<u8>, FileAttrs)>>>,
        readdir_result: Mutex<Option<anyhow::Result<Vec<ReaddirEntry>>>>,
        stat_batch_result: Mutex<Option<anyhow::Result<Vec<Result<FileAttrs, FsError>>>>>,
        read_chunks_result: Mutex<Option<anyhow::Result<ChunkReadResult>>>,
        merkle_drill_result: Mutex<Option<anyhow::Result<MerkleDrillResult>>>,
        read_chunks_called: Mutex<u32>,
    }

    #[allow(dead_code)]
    impl MockRemote {
        fn new() -> Self {
            Self {
                lookup_result: Mutex::new(None),
                readdir_result: Mutex::new(None),
                stat_batch_result: Mutex::new(None),
                read_chunks_result: Mutex::new(None),
                merkle_drill_result: Mutex::new(None),
                read_chunks_called: Mutex::new(0),
            }
        }

        async fn set_lookup(&self, result: anyhow::Result<(Vec<u8>, FileAttrs)>) {
            *self.lookup_result.lock().await = Some(result);
        }

        async fn set_readdir(&self, result: anyhow::Result<Vec<ReaddirEntry>>) {
            *self.readdir_result.lock().await = Some(result);
        }

        async fn set_stat_batch(&self, result: anyhow::Result<Vec<Result<FileAttrs, FsError>>>) {
            *self.stat_batch_result.lock().await = Some(result);
        }

        async fn set_read_chunks(&self, result: anyhow::Result<ChunkReadResult>) {
            *self.read_chunks_result.lock().await = Some(result);
        }

        async fn set_merkle_drill(&self, result: anyhow::Result<MerkleDrillResult>) {
            *self.merkle_drill_result.lock().await = Some(result);
        }

        async fn get_read_chunks_call_count(&self) -> u32 {
            *self.read_chunks_called.lock().await
        }
    }

    #[async_trait]
    impl RemoteShare for MockRemote {
        async fn lookup(
            &self,
            _parent_handle: &[u8],
            _name: &str,
        ) -> anyhow::Result<(Vec<u8>, FileAttrs)> {
            self.lookup_result
                .lock()
                .await
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no lookup result set")))
        }

        async fn readdir(&self, _handle: &[u8]) -> anyhow::Result<Vec<ReaddirEntry>> {
            self.readdir_result
                .lock()
                .await
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no readdir result set")))
        }

        async fn stat_batch(
            &self,
            _handles: Vec<Vec<u8>>,
        ) -> anyhow::Result<Vec<Result<FileAttrs, FsError>>> {
            self.stat_batch_result
                .lock()
                .await
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no stat_batch result set")))
        }

        async fn read_chunks(
            &self,
            _handle: &[u8],
            _start_chunk: u32,
            _chunk_count: u32,
        ) -> anyhow::Result<ChunkReadResult> {
            *self.read_chunks_called.lock().await += 1;
            self.read_chunks_result
                .lock()
                .await
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no read_chunks result set")))
        }

        async fn merkle_drill(
            &self,
            _handle: &[u8],
            _level: u32,
            _parent_indices: &[u32],
        ) -> anyhow::Result<MerkleDrillResult> {
            self.merkle_drill_result
                .lock()
                .await
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no merkle_drill result set")))
        }
    }

    fn make_file_attrs(size: u64, root_hash: [u8; 32]) -> FileAttrs {
        FileAttrs {
            size,
            root_hash: root_hash.to_vec(),
            file_type: rift_protocol::messages::FileType::Regular as i32,
            nlinks: 1,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn non_cached_read_fetches_from_server() {
        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView::new(remote.clone());

        let handle = b"test_file";
        let root_hash = [0xAB; 32];
        let content = b"hello world";

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                content.len() as u64,
                root_hash,
            ))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                hashes: vec![root_hash.to_vec()],
                sizes: vec![],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: root_hash,
                    data: content.to_vec(),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        let result = view.read(handle, 0, content.len() as u64, None).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content);

        let call_count = remote.get_read_chunks_call_count().await;
        assert_eq!(
            call_count, 1,
            "read_chunks should be called exactly once for non-cached read"
        );
    }

    #[tokio::test]
    async fn cached_read_hits_cache_when_manifest_available() {
        let cache = FileCache::open_in_memory().await.unwrap();
        let cache = Arc::new(cache);

        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView {
            remote: remote.clone(),
            cache: Some(cache.clone()),
        };

        let handle = b"cached_file";
        let root_hash = Blake3Hash::new(b"test-root");
        let content = b"cached content here";

        cache
            .put_manifest(
                handle,
                &Manifest {
                    root: root_hash.clone(),
                    chunks: vec![ChunkInfo {
                        index: 0,
                        offset: 0,
                        length: content.len() as u64,
                        hash: *root_hash.as_bytes(),
                    }],
                },
            )
            .await
            .unwrap();

        cache
            .put_chunk(root_hash.as_bytes(), content)
            .await
            .unwrap();

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                content.len() as u64,
                *root_hash.as_bytes(),
            ))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                hashes: vec![root_hash.as_bytes().to_vec()],
                sizes: vec![],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![],
                merkle_root: vec![],
            }))
            .await;

        let result = view.read(handle, 0, content.len() as u64, None).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content);

        let call_count = remote.get_read_chunks_call_count().await;
        assert_eq!(
            call_count, 0,
            "read_chunks should NOT be called when cache has data"
        );
    }

    #[tokio::test]
    async fn cached_read_falls_back_to_server_when_no_manifest() {
        let cache = FileCache::open_in_memory().await.unwrap();
        let cache = Arc::new(cache);

        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView {
            remote: remote.clone(),
            cache: Some(cache.clone()),
        };

        let handle = b"uncached_file";
        let root_hash = [0xCD; 32];
        let content = b"server content";

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                content.len() as u64,
                root_hash,
            ))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                hashes: vec![root_hash.to_vec()],
                sizes: vec![],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: root_hash,
                    data: content.to_vec(),
                }],
                merkle_root: root_hash.to_vec(),
            }))
            .await;

        let result = view.read(handle, 0, content.len() as u64, None).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content);

        let call_count = remote.get_read_chunks_call_count().await;
        assert_eq!(
            call_count, 1,
            "read_chunks should be called when cache has no manifest"
        );
    }

    #[tokio::test]
    async fn cached_read_falls_back_when_root_hash_mismatch() {
        let cache = FileCache::open_in_memory().await.unwrap();
        let cache = Arc::new(cache);

        let remote = Arc::new(MockRemote::new());
        let view = RiftShareView {
            remote: remote.clone(),
            cache: Some(cache.clone()),
        };

        let handle = b"stale_cache_file";
        let old_root = Blake3Hash::new(b"old-root");
        let new_root = [0xEF; 32];
        let content = b"new content";

        cache
            .put_manifest(
                handle,
                &Manifest {
                    root: old_root,
                    chunks: vec![ChunkInfo {
                        index: 0,
                        offset: 0,
                        length: 100,
                        hash: [0x00; 32],
                    }],
                },
            )
            .await
            .unwrap();

        remote
            .set_stat_batch(Ok(vec![Ok(make_file_attrs(
                content.len() as u64,
                new_root,
            ))]))
            .await;
        remote
            .set_merkle_drill(Ok(MerkleDrillResult {
                hashes: vec![new_root.to_vec()],
                sizes: vec![],
            }))
            .await;
        remote
            .set_read_chunks(Ok(ChunkReadResult {
                chunks: vec![ChunkData {
                    index: 0,
                    length: content.len() as u64,
                    hash: new_root,
                    data: content.to_vec(),
                }],
                merkle_root: new_root.to_vec(),
            }))
            .await;

        let result = view.read(handle, 0, content.len() as u64, None).await;

        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            content,
            "should fetch from server when root hash differs"
        );

        let call_count = remote.get_read_chunks_call_count().await;
        assert_eq!(
            call_count, 1,
            "should fall back to server on root hash mismatch"
        );
    }
}
