use arc_swap::ArcSwap;

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

/// Returns `true` if `err` represents a transport-level connection failure
/// that warrants a reconnect attempt.
///
/// Uses typed downcasting against [`rift_transport::TransportError`] rather than
/// substring matching on error messages. This prevents false positives such as
/// application-level errors whose messages happen to contain words like "stream",
/// "closed", or "connection" from triggering spurious reconnects.
///
/// [`rift_common::FsError`] values (POSIX filesystem errors returned by the server)
/// are never connection errors — they are application-level responses.
fn is_connection_error(err: &anyhow::Error) -> bool {
    // POSIX filesystem errors from the server are never connection errors.
    if err.downcast_ref::<FsError>().is_some() {
        return false;
    }

    // Check typed transport errors. This covers all QUIC/TLS/IO failures
    // that indicate the connection is broken and a reconnect should be tried.
    if let Some(te) = err.downcast_ref::<rift_transport::TransportError>() {
        return matches!(
            te,
            rift_transport::TransportError::ConnectionClosed
                | rift_transport::TransportError::StreamClosed
                | rift_transport::TransportError::QuicConnection(_)
                | rift_transport::TransportError::QuicRead(_)
                | rift_transport::TransportError::QuicWrite(_)
                | rift_transport::TransportError::QuicConnect(_)
                | rift_transport::TransportError::Io(_)
        );
    }

    // Unknown error types are not treated as connection errors.
    // This is the safe default: an unrecognized error is surfaced to the
    // caller rather than silently retried.
    false
}

/// A `RiftClient` wrapper that transparently reconnects on connection errors.
///
/// ## Concurrency model
///
/// [`ArcSwap`] provides lock-free concurrent reads: every operation loads the
/// current `Arc<RiftClient>` atomically (a single pointer-width CAS) and
/// proceeds without holding any lock.
///
/// Reconnects are serialized by [`reconnect_lock`]. Once the new connection is
/// established, `ArcSwap::store` atomically publishes it. Operations that were
/// using the old (broken) connection will fail and retry, picking up the new
/// client on the next iteration.
///
/// This means reads are **never** blocked by a reconnect in progress — a key
/// difference from a `RwLock`-based design, where a write lock held during the
/// QUIC handshake (potentially seconds) would stall all concurrent FUSE ops.
pub struct ReconnectingClient {
    client: ArcSwap<RiftClient<rift_transport::QuicConnection>>,
    /// Ensures only one reconnect attempt runs at a time.
    reconnect_lock: Mutex<()>,
}

impl ReconnectingClient {
    #[must_use]
    pub fn new(client: RiftClient<rift_transport::QuicConnection>) -> Self {
        Self {
            client: ArcSwap::from_pointee(client),
            reconnect_lock: Mutex::new(()),
        }
    }

    async fn with_reconnect<F, Fut, T>(&self, make_op: F) -> anyhow::Result<T>
    where
        F: Fn(Arc<RiftClient<rift_transport::QuicConnection>>) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<T>>,
    {
        let mut attempts = 0;

        loop {
            // Lock-free atomic load — no lock held during the network RPC.
            let client = self.client.load_full();

            match make_op(client).await {
                Ok(result) => return Ok(result),
                Err(e) if !is_connection_error(&e) => return Err(e),
                Err(e) if attempts >= MAX_RETRIES => {
                    tracing::warn!("operation failed after {} retries: {}", MAX_RETRIES, e);
                    return Err(e);
                }
                Err(e) => {
                    attempts += 1;
                    // Saturating shift avoids overflow if attempts ever exceeds 63.
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

                    // Serialize reconnects: only one task re-establishes the
                    // connection at a time. The lock is NOT held while the new
                    // connection is being set up — the await happens with the
                    // lock held only briefly to read the current client, then
                    // released during the handshake, then re-acquired to store.
                    //
                    // Actually simpler: hold the lock for the whole reconnect.
                    // Concurrent ops use the old (broken) client and will retry.
                    let _guard = self.reconnect_lock.lock().await;
                    let old = self.client.load_full();
                    match old.reconnect().await {
                        Ok(new_client) => self.client.store(Arc::new(new_client)),
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
        let _guard = self.reconnect_lock.lock().await;
        let old = self.client.load_full();
        let new_client = old.reconnect().await?;
        self.client.store(Arc::new(new_client));
        Ok(())
    }

    pub fn close_connection_for_test(&self) {
        self.client.load().close_connection();
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
    fn is_connection_error_returns_true_for_transport_errors() {
        use rift_transport::TransportError;

        let conn_closed = anyhow::Error::new(TransportError::ConnectionClosed);
        assert!(is_connection_error(&conn_closed));

        let stream_closed = anyhow::Error::new(TransportError::StreamClosed);
        assert!(is_connection_error(&stream_closed));

        let io_err = anyhow::Error::new(TransportError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        )));
        assert!(is_connection_error(&io_err));
    }

    #[test]
    fn is_connection_error_returns_false_for_non_transport_string_errors() {
        assert!(!is_connection_error(&anyhow::anyhow!("connection timed out")));
        assert!(!is_connection_error(&anyhow::anyhow!("stream limit exceeded")));
        assert!(!is_connection_error(&anyhow::anyhow!("QUIC error in log message")));
    }

    #[test]
    fn is_connection_error_walks_chain_through_context_wrappers() {
        use rift_transport::TransportError;
        let err = anyhow::Error::from(TransportError::ConnectionClosed)
            .context("open stream")
            .context("lookup");
        assert!(
            is_connection_error(&err),
            "must recognise TransportError wrapped in anyhow context layers"
        );
    }

    #[test]
    fn is_connection_error_uses_typed_downcast_for_transport_errors() {
        use rift_transport::TransportError;

        let conn_closed = anyhow::Error::new(TransportError::ConnectionClosed);
        assert!(is_connection_error(&conn_closed));

        let codec_err = anyhow::Error::new(TransportError::Codec(
            rift_protocol::codec::CodecError::InvalidVarint,
        ));
        assert!(!is_connection_error(&codec_err), "Codec must NOT trigger reconnect");

        let ambiguous = anyhow::anyhow!("stream limit exceeded in application layer");
        assert!(!is_connection_error(&ambiguous), "'stream' substring must not trigger reconnect");
    }
}
