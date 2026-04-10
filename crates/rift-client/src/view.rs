//! Defines the `ShareView` trait, the filesystem-level abstraction that the FUSE
//! implementation uses.

use crate::remote::RemoteShare;
use async_trait::async_trait;
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, ReaddirEntry};
use std::sync::Arc;

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
    async fn getattr(&self, handle: &[u8]) -> Result<FileAttrs, FsError> {
        self.remote
            .stat_batch(vec![handle.to_vec()])
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))?
            .remove(0)
    }

    async fn lookup(&self, parent: &[u8], name: &str) -> Result<(Vec<u8>, FileAttrs), FsError> {
        self.remote
            .lookup(parent, name)
            .await
            .map_err(|e| e.downcast::<FsError>().unwrap_or(FsError::Io))
    }

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
}
