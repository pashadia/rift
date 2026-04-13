//! Defines the `ShareView` trait, the filesystem-level abstraction that the FUSE
//! implementation uses.

use crate::remote::RemoteShare;
use async_trait::async_trait;
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, ReaddirEntry};
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
pub struct RiftShareView<R: RemoteShare> {
    remote: Arc<R>,
}

impl<R: RemoteShare> RiftShareView<R> {
    pub fn new(remote: Arc<R>) -> Self {
        Self { remote }
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
        cached_root_hash: Option<&[u8]>,
    ) -> Result<Vec<u8>, FsError> {
        let attrs = self
            .remote
            .stat_batch(vec![handle.to_vec()])
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?
            .remove(0)?;

        if attrs.root_hash.is_empty() {
            return Err(FsError::Io);
        }
        let merkle_root = attrs.root_hash;

        if attrs.size == 0 {
            return Ok(vec![]);
        }

        if let Some(cached) = cached_root_hash {
            if cached == merkle_root.as_slice() {
                return Ok(vec![]);
            }
        }

        let file_size = attrs.size;
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
