use crate::client::{ChunkReadResult, MerkleDrillResult, RiftClient};
use crate::remote::RemoteShare;
use async_trait::async_trait;
use rift_common::FsError;
use rift_protocol::messages::{FileAttrs, ReaddirEntry};
use std::sync::Arc;
use tokio::sync::Mutex;
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

pub struct ReconnectingClient {
    client: Arc<Mutex<RiftClient<rift_transport::QuicConnection>>>,
}

impl ReconnectingClient {
    pub fn new(client: RiftClient<rift_transport::QuicConnection>) -> Self {
        Self {
            client: Arc::new(Mutex::new(client)),
        }
    }

    async fn with_reconnect<F, Fut, T>(&self, make_op: F) -> anyhow::Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<T>>,
    {
        let mut attempts = 0;

        let result = loop {
            match make_op().await {
                Ok(result) => break Ok(result),
                Err(e) if !is_connection_error(&e) => {
                    break Err(e);
                }
                Err(e) if attempts >= MAX_RETRIES => {
                    tracing::warn!("operation failed after {} retries: {}", MAX_RETRIES, e);
                    break Err(e);
                }
                Err(e) => {
                    attempts += 1;
                    let backoff_ms = BASE_BACKOFF_MS * 2u64.pow(attempts - 1);
                    tracing::warn!(
                        "connection error (attempt {}/{}): {}. retrying in {}ms",
                        attempts,
                        MAX_RETRIES,
                        e,
                        backoff_ms
                    );
                    tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;

                    let mut guard = self.client.lock().await;
                    match guard.reconnect().await {
                        Ok(new_client) => {
                            *guard = new_client;
                        }
                        Err(reconnect_err) => {
                            tracing::error!("reconnect failed: {}", reconnect_err);
                            break Err(reconnect_err);
                        }
                    }
                    drop(guard);
                }
            }
        };

        result
    }

    pub async fn reconnect(&self) -> anyhow::Result<()> {
        let mut guard = self.client.lock().await;
        let new_client = guard.reconnect().await?;
        *guard = new_client;
        Ok(())
    }

    pub fn close_connection_for_test(&self) {
        let guard = self.client.try_lock();
        if let Ok(guard) = guard {
            guard.close_connection();
        }
    }
}

#[async_trait]
impl RemoteShare for ReconnectingClient {
    async fn lookup(&self, parent: Uuid, name: &str) -> anyhow::Result<(Uuid, FileAttrs)> {
        let name = name.to_string();
        let client = self.client.clone();
        self.with_reconnect(move || {
            let name = name.clone();
            let client = client.clone();
            async move {
                let client = client.lock().await;
                client.lookup(parent, &name).await
            }
        })
        .await
    }

    async fn readdir(&self, handle: Uuid) -> anyhow::Result<Vec<ReaddirEntry>> {
        let client = self.client.clone();
        self.with_reconnect(move || {
            let client = client.clone();
            async move {
                let client = client.lock().await;
                client.readdir(handle).await
            }
        })
        .await
    }

    async fn stat_batch(
        &self,
        handles: Vec<Uuid>,
    ) -> anyhow::Result<Vec<Result<FileAttrs, FsError>>> {
        let client = self.client.clone();
        self.with_reconnect(move || {
            let handles = handles.clone();
            let client = client.clone();
            async move {
                let client = client.lock().await;
                client.stat_batch(handles).await
            }
        })
        .await
    }

    async fn read_chunks(
        &self,
        handle: Uuid,
        start_chunk: u32,
        chunk_count: u32,
    ) -> anyhow::Result<ChunkReadResult> {
        let client = self.client.clone();
        self.with_reconnect(move || {
            let client = client.clone();
            async move {
                let client = client.lock().await;
                client.read_chunks(handle, start_chunk, chunk_count).await
            }
        })
        .await
    }

    async fn merkle_drill(
        &self,
        handle: Uuid,
        level: u32,
        subtrees: &[u32],
    ) -> anyhow::Result<MerkleDrillResult> {
        let subtrees = subtrees.to_vec();
        let client = self.client.clone();
        self.with_reconnect(move || {
            let subtrees = subtrees.clone();
            let client = client.clone();
            async move {
                let client = client.lock().await;
                let resp = client.merkle_drill(handle, level, &subtrees).await?;
                Ok(resp.into())
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
