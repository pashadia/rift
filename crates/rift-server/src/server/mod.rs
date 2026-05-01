//! QUIC connection acceptance and per-stream dispatch.
//!
//! The entry point is [`accept_loop`], which accepts incoming connections and
//! spawns a task per connection.  Each connection task accepts streams and
//! spawns a task per stream.  Each stream task reads the first frame to
//! determine the request type and delegates to the appropriate handler.
//!
//! # Protocol flow
//!
//! ```text
//! Client                          Server
//!   │─── open stream 0 ──────────►│
//!   │─── RIFT_HELLO ─────────────►│  version check + send_welcome
//!   │◄── RIFT_WELCOME ────────────│
//!   │
//!   │─── open stream N ──────────►│  (one stream per operation)
//!   │─── STAT_REQUEST ───────────►│  handler::stat_response
//!   │◄── STAT_RESPONSE ───────────│
//! ```

pub mod context;
mod dispatch;

use tracing::{debug_span, instrument, warn, Instrument};

use rift_transport::{RiftConnection, RiftListener, TransportError};

use crate::handler::merkle_cache_trait::MerkleCache;
use dispatch::serve_connection;

// Re-export for backward compatibility with external call sites.
pub use context::recv_request;
pub use context::IncomingRequest;
pub use context::RequestContext;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Accept connections in a loop and serve each one in a background task.
///
/// Uses a Merkle cache database for computing root hashes. Pass `None` for `db`
/// to disable caching (root hashes will still be computed on-demand).
///
/// Generic over any [`RiftListener`] to allow testing with in-memory transports.
#[instrument(skip(listener, ctx), fields(share = %ctx.share.display(), listen_addr = %listener.local_addr()))]
pub async fn accept_loop<L, M: MerkleCache + 'static>(
    listener: L,
    ctx: RequestContext<M>,
) -> anyhow::Result<()>
where
    L: RiftListener,
    L::Connection: 'static,
    <L::Connection as RiftConnection>::Stream: 'static,
{
    loop {
        match listener.accept().await {
            Ok(conn) => {
                let ctx = ctx.clone();
                let peer = conn.peer_fingerprint().to_string();
                let conn_span = debug_span!("server.connection", peer = %peer);
                tokio::spawn(
                    async move {
                        serve_connection(conn, ctx).await;
                    }
                    .instrument(conn_span),
                );
            }
            Err(TransportError::ConnectionClosed) => break,
            Err(e) => {
                warn!("accept error: {e}");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rift_common::crypto::Chunker;
    use rift_transport::{
        client_handshake, InMemoryConnector, InMemoryListener, RiftConnection,
        RIFT_PROTOCOL_VERSION,
    };

    use super::context::RequestContext;
    use super::*;
    use crate::handle::HandleDatabase;
    use crate::handler::merkle_cache_trait::NoopCache;

    /// Helper: build a minimal server config pointing at a real temp directory.
    fn make_share() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // put a token file so the share isn't empty
        std::fs::write(root.join("hello.txt"), b"hello").unwrap();
        (dir, root)
    }

    /// Helper: spin up `accept_loop` with an InMemoryListener and return the
    /// connector so tests can open connections.
    fn start_in_memory_server(
        share: std::path::PathBuf,
    ) -> (
        tokio::task::JoinHandle<anyhow::Result<()>>,
        InMemoryConnector,
    ) {
        let (listener, connector) = InMemoryListener::new("test-server-fp", "test-client-fp");
        let db: Arc<NoopCache> = Arc::new(NoopCache);
        let handle_db = Arc::new(HandleDatabase::new());
        let ctx = RequestContext {
            share,
            db,
            handle_db,
            chunker: Chunker::default(),
        };
        let handle = tokio::spawn(accept_loop(listener, ctx));
        (handle, connector)
    }

    #[tokio::test]
    async fn accept_loop_accepts_and_handles_a_connection() {
        let (_dir, share) = make_share();
        let (_server_handle, connector) = start_in_memory_server(share);

        // Open a client-side connection and run the RIFT handshake.
        let client_conn = connector.connect().unwrap();
        let mut ctrl = client_conn.open_stream().await.unwrap();

        let welcome = client_handshake(&mut ctrl, "demo", &[]).await.unwrap();

        // Handshake succeeded: server assigned a root handle (UUID bytes).
        assert_eq!(
            welcome.protocol_version, RIFT_PROTOCOL_VERSION,
            "server must echo the protocol version"
        );
        assert_eq!(
            welcome.root_handle.len(),
            16,
            "root_handle must be a 16-byte UUID"
        );

        // Drop the connector; accept_loop will see ConnectionClosed and exit.
        drop(connector);
    }

    #[tokio::test]
    async fn accept_loop_exits_when_listener_closes() {
        let (_dir, share) = make_share();
        let (server_handle, connector) = start_in_memory_server(share);

        // Dropping the connector closes the channel, which causes the
        // InMemoryListener::accept() to return ConnectionClosed.
        drop(connector);

        // accept_loop must terminate within a reasonable timeout.
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), server_handle)
            .await
            .expect("accept_loop did not exit within 2 s after listener closed");

        // The task itself must return Ok(()).
        assert!(
            result.unwrap().is_ok(),
            "accept_loop must return Ok when listener closes"
        );
    }
}
