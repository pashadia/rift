use crate::client::{ChunkReadResult, MerkleDrillResult, RiftClient};
use crate::remote::RemoteShare;
use async_trait::async_trait;
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, ReaddirEntry};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

const MAX_RETRIES: u32 = 5;
const BASE_BACKOFF_MS: u64 = 100;

fn is_connection_error(err: &anyhow::Error) -> bool {
    if err.downcast_ref::<FsError>().is_some() {
        return false;
    }

    let msg = err.to_string().to_lowercase();
    msg.contains("connection")
        || msg.contains("timeout")
        || msg.contains("closed")
        || msg.contains("stream")
        || msg.contains("quic")
        || msg.contains("network")
        || msg.contains("reset")
        || msg.contains("refused")
}

/// A `RiftClient` wrapper that transparently reconnects on connection errors.
///
/// ## Concurrency model
///
/// Normal operations (lookup, readdir, stat_batch, read_chunks, merkle_drill)
/// run **concurrently** — no lock is held during the network RPC itself:
///
/// 1. Acquire a **read** lock briefly to clone the inner `Arc<RiftClient>`.
/// 2. Drop the read lock immediately.
/// 3. Run the network operation with the cloned Arc (no lock held).
///
/// The reconnect path serializes via an exclusive **write** lock:
///
/// 1. Acquire write lock (waits for all in-flight ops to drain).
/// 2. Call `reconnect()` to get a new client.
/// 3. Swap the inner Arc.
/// 4. Drop write lock.
///
/// This means concurrent `stat`/`readdir`/`read` calls no longer block each
/// other, which was the critical bottleneck for parallel FUSE workloads such
/// as `grep -r`, `find`, and `ls -R`.
pub struct ReconnectingClient {
    client: Arc<RwLock<Arc<RiftClient<rift_transport::QuicConnection>>>>,
}

impl ReconnectingClient {
    #[must_use]
    pub fn new(client: RiftClient<rift_transport::QuicConnection>) -> Self {
        Self {
            client: Arc::new(RwLock::new(Arc::new(client))),
        }
    }

    async fn with_reconnect<F, Fut, T>(&self, make_op: F) -> anyhow::Result<T>
    where
        F: Fn(Arc<RiftClient<rift_transport::QuicConnection>>) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<T>>,
    {
        let mut attempts = 0;

        loop {
            // Clone the Arc while briefly holding the read lock.
            // The lock is dropped before the network operation runs,
            // allowing other operations to proceed concurrently.
            let client = Arc::clone(&*self.client.read().await);

            match make_op(client).await {
                Ok(result) => return Ok(result),
                Err(e) if !is_connection_error(&e) => return Err(e),
                Err(e) if attempts >= MAX_RETRIES => {
                    tracing::warn!("operation failed after {} retries: {}", MAX_RETRIES, e);
                    return Err(e);
                }
                Err(e) => {
                    attempts += 1;
                    // Saturating shift avoids overflow if attempts ever exceeds 63
                    let backoff_ms =
                        BASE_BACKOFF_MS.saturating_mul(1u64 << (attempts - 1).min(10));
                    tracing::warn!(
                        "connection error (attempt {}/{}): {}. retrying in {}ms",
                        attempts,
                        MAX_RETRIES,
                        e,
                        backoff_ms
                    );
                    tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;

                    // Write lock: exclusive — waits for all in-flight ops to drain,
                    // then replaces the client Arc atomically.
                    let mut guard = self.client.write().await;
                    match guard.reconnect().await {
                        Ok(new_client) => *guard = Arc::new(new_client),
                        Err(reconnect_err) => {
                            tracing::error!("reconnect failed: {}", reconnect_err);
                            return Err(reconnect_err);
                        }
                    }
                }
            }
        }
    }

    pub async fn reconnect(&self) -> anyhow::Result<()> {
        let mut guard = self.client.write().await;
        let new_client = guard.reconnect().await?;
        *guard = Arc::new(new_client);
        Ok(())
    }

    pub fn close_connection_for_test(&self) {
        if let Ok(guard) = self.client.try_read() {
            guard.close_connection();
        }
    }
}

#[async_trait]
impl RemoteShare for ReconnectingClient {
    async fn lookup(&self, parent: Uuid, name: &str) -> anyhow::Result<(Uuid, FileAttrs)> {
        let name = name.to_string();
        self.with_reconnect(move |client| {
            let name = name.clone();
            async move { client.lookup(parent, &name).await }
        })
        .await
    }

    async fn readdir(&self, handle: Uuid) -> anyhow::Result<Vec<ReaddirEntry>> {
        self.with_reconnect(move |client| async move { client.readdir(handle).await })
            .await
    }

    async fn stat_batch(
        &self,
        handles: Vec<Uuid>,
    ) -> anyhow::Result<Vec<Result<FileAttrs, FsError>>> {
        self.with_reconnect(move |client| {
            let handles = handles.clone();
            async move { client.stat_batch(handles).await }
        })
        .await
    }

    async fn read_chunks(
        &self,
        handle: Uuid,
        start_chunk: u32,
        chunk_count: u32,
    ) -> anyhow::Result<ChunkReadResult> {
        self.with_reconnect(move |client| async move {
            client.read_chunks(handle, start_chunk, chunk_count).await
        })
        .await
    }

    async fn merkle_drill(&self, handle: Uuid, hash: &[u8]) -> anyhow::Result<MerkleDrillResult> {
        let hash = hash.to_vec();
        self.with_reconnect(move |client| {
            let hash = hash.clone();
            async move {
                let resp = client.merkle_drill(handle, &hash).await?;
                Ok(resp.into())
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_read_locks_do_not_block_each_other() {
        use std::time::{Duration, Instant};
        use tokio::sync::{Barrier, RwLock};

        let rwlock = Arc::new(RwLock::new(Arc::new(42u64)));
        let barrier = Arc::new(Barrier::new(2));
        let start = Instant::now();

        let r1 = rwlock.clone();
        let b1 = barrier.clone();
        let t1 = tokio::spawn(async move {
            let val = Arc::clone(&*r1.read().await);
            b1.wait().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            *val
        });

        let r2 = rwlock.clone();
        let b2 = barrier.clone();
        let t2 = tokio::spawn(async move {
            let val = Arc::clone(&*r2.read().await);
            b2.wait().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            *val
        });

        let (v1, v2) = tokio::join!(t1, t2);
        let elapsed = start.elapsed();
        assert_eq!(v1.unwrap(), 42);
        assert_eq!(v2.unwrap(), 42);
        assert!(
            elapsed < Duration::from_millis(180),
            "concurrent reads must not serialize, took {:?}",
            elapsed
        );
    }

    #[test]
    fn is_connection_error_returns_false_for_fs_error() {
        let not_found = anyhow::Error::new(FsError::NotFound);
        assert!(!is_connection_error(&not_found));

        let perm_denied = anyhow::Error::new(FsError::PermissionDenied);
        assert!(!is_connection_error(&perm_denied));

        let io_error = anyhow::Error::new(FsError::Io);
        assert!(!is_connection_error(&io_error));
    }

    #[test]
    fn is_connection_error_returns_true_for_connection_errors() {
        let timeout = anyhow::anyhow!("connection timed out");
        assert!(is_connection_error(&timeout));

        let closed = anyhow::anyhow!("connection closed");
        assert!(is_connection_error(&closed));

        let refused = anyhow::anyhow!("connection refused");
        assert!(is_connection_error(&refused));
    }

    #[test]
    fn is_connection_error_returns_true_for_quic_errors() {
        let quic_err = anyhow::anyhow!("QUIC connection error: timed out");
        assert!(is_connection_error(&quic_err));
    }
}
