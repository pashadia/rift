//! Defines the `RemoteShare` trait, the network protocol-level abstraction.

use async_trait::async_trait;
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, ReaddirEntry};

/// The `RemoteShare` trait is a pure, 1:1 mapping of the network protocol's
/// capabilities. It speaks in terms of handles and protocol-level operations.
/// It is the boundary for all network communication.
#[async_trait]
pub trait RemoteShare: Send + Sync + 'static {
    /// Corresponds to a LOOKUP_REQUEST.
    async fn lookup(
        &self,
        parent_handle: &[u8],
        name: &str,
    ) -> anyhow::Result<(Vec<u8>, FileAttrs)>;

    /// Corresponds to a READDIR_REQUEST.
    async fn readdir(&self, handle: &[u8]) -> anyhow::Result<Vec<ReaddirEntry>>;

    /// Corresponds to a batch STAT_REQUEST.
    async fn stat_batch(
        &self,
        handles: Vec<Vec<u8>>,
    ) -> anyhow::Result<Vec<Result<FileAttrs, FsError>>>;
}
