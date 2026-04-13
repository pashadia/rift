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
