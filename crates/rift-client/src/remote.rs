use async_trait::async_trait;
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, ReaddirEntry};
use uuid::Uuid;

use crate::client::{ChunkReadResult, MerkleDrillResult};
/// The `RemoteShare` trait is a pure, 1:1 mapping of the network protocol's
/// capabilities. It speaks in terms of UUID handles and protocol-level operations.
/// It is the boundary for all network communication.
#[async_trait]
pub trait RemoteShare: Send + Sync + 'static {
    /// Corresponds to a LOOKUP_REQUEST.
    async fn lookup(&self, parent_handle: Uuid, name: &str) -> anyhow::Result<(Uuid, FileAttrs)>;

    /// Corresponds to a READDIR_REQUEST.
    async fn readdir(&self, handle: Uuid) -> anyhow::Result<Vec<ReaddirEntry>>;

    /// Corresponds to a batch STAT_REQUEST.
    async fn stat_batch(
        &self,
        handles: Vec<Uuid>,
    ) -> anyhow::Result<Vec<Result<FileAttrs, FsError>>>;

    /// Reads chunks from a file.
    async fn read_chunks(
        &self,
        handle: Uuid,
        start_chunk: u32,
        chunk_count: u32,
    ) -> anyhow::Result<ChunkReadResult>;

    /// Drills the Merkle tree to get children of a specific node.
    /// `hash`: empty = request root's children, otherwise the hash to query.
    async fn merkle_drill(&self, handle: Uuid, hash: &[u8]) -> anyhow::Result<MerkleDrillResult>;
}
