//! Shared context and request types for the RIFT server.
//!
//! [`RequestContext`] bundles the dependencies that every stream handler needs,
//! and [`IncomingRequest`] represents the first frame received on a new stream.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use rift_common::crypto::Chunker;
use rift_transport::RiftStream;
use tracing::debug;

use crate::handle::HandleDatabase;
use crate::handler::merkle_cache_trait::MerkleCache;

// ---------------------------------------------------------------------------
// RequestContext
// ---------------------------------------------------------------------------

/// Shared context carried by every stream handler.
///
/// Bundles the four dependency parameters (`share`, `db`, `handle_db`,
/// `chunker`) that were previously threaded through `accept_loop`,
/// `serve_connection`, and `handle_stream` as separate arguments.
pub struct RequestContext<M: MerkleCache> {
    pub share: PathBuf,
    pub db: Arc<M>,
    pub handle_db: Arc<HandleDatabase>,
    pub chunker: Chunker,
}

impl<M: MerkleCache> RequestContext<M> {
    /// Returns a reference to the inner Merkle cache.
    ///
    /// This is a convenience accessor that replaces the previous
    /// `db.as_ref()` calls at handler call sites.
    pub fn db(&self) -> &M {
        self.db.as_ref()
    }
}

impl<M: MerkleCache> Clone for RequestContext<M> {
    fn clone(&self) -> Self {
        Self {
            share: self.share.clone(),
            db: self.db.clone(),
            handle_db: self.handle_db.clone(),
            chunker: self.chunker,
        }
    }
}

// ---------------------------------------------------------------------------
// IncomingRequest
// ---------------------------------------------------------------------------

/// The first frame received on a new stream.
pub struct IncomingRequest {
    pub type_id: u8,
    pub payload: Bytes,
}

/// Receive the first frame from a stream, or return `Ok(None)` for empty streams.
pub async fn recv_request<S: RiftStream>(
    stream: &mut S,
) -> anyhow::Result<Option<IncomingRequest>> {
    match stream.recv_frame().await {
        Ok(Some((type_id, payload))) => Ok(Some(IncomingRequest { type_id, payload })),
        Ok(None) => {
            debug!("empty stream — ignoring");
            Ok(None)
        }
        Err(e) => {
            debug!(error = %e, "recv_frame failed");
            Err(e.into())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use rift_transport::connection::InMemoryConnection;
    use rift_transport::RiftConnection;

    use super::*;

    #[tokio::test]
    async fn recv_request_returns_some_for_valid_frame() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        // Client sends a frame
        client_stream.send_frame(0x42, b"hello").await.unwrap();
        client_stream.finish_send().await.unwrap();

        let request = recv_request(&mut server_stream).await.unwrap();
        assert!(request.is_some(), "recv_request should return Some");
        let req = request.unwrap();
        assert_eq!(req.type_id, 0x42);
        assert_eq!(&req.payload[..], b"hello");
    }

    #[tokio::test]
    async fn recv_request_returns_none_for_empty_stream() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        // Client finishes without sending any frames
        client_stream.finish_send().await.unwrap();

        let request = recv_request(&mut server_stream).await.unwrap();
        assert!(
            request.is_none(),
            "recv_request should return None for empty stream"
        );
    }
}
