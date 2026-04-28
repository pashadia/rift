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

use std::path::PathBuf;
use std::sync::Arc;

use prost::Message as _;
use tracing::{debug, debug_span, instrument, warn, Instrument};

use rift_common::crypto::Chunker;
use rift_protocol::messages::{msg, ErrorCode, ErrorDetail, RiftHello, RiftWelcome, ShareInfo};
use rift_transport::{
    send_welcome, RiftConnection, RiftListener, RiftStream, TransportError, RIFT_PROTOCOL_VERSION,
};

use crate::{handle::HandleDatabase, handler, handler::merkle_cache_trait::MerkleCache};

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
// Per-connection loop
// ---------------------------------------------------------------------------

#[instrument(skip(conn, ctx), fields(share = %ctx.share.display(), peer = %conn.peer_fingerprint()))]
async fn serve_connection<C, M: MerkleCache + 'static>(conn: C, ctx: RequestContext<M>)
where
    C: RiftConnection,
    C::Stream: 'static,
{
    debug!("connection established");

    // TODO(v1): authorise peer fingerprint against per-share permission files
    // before accepting any streams.

    loop {
        match conn.accept_stream().await {
            Ok(stream) => {
                let ctx = ctx.clone();
                let stream_span = debug_span!("server.stream", share = %ctx.share.display());
                tokio::spawn(
                    async move {
                        if let Err(e) = handle_stream(stream, ctx).await {
                            debug!("stream error: {}", e);
                        }
                    }
                    .instrument(stream_span),
                );
            }
            Err(_) => {
                debug!("connection closed");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-stream dispatch
// ---------------------------------------------------------------------------

#[instrument(skip_all, fields(share = %ctx.share.display()), err)]
async fn handle_stream<S, M: MerkleCache>(
    mut stream: S,
    ctx: RequestContext<M>,
) -> anyhow::Result<()>
where
    S: RiftStream,
{
    let (type_id, payload) = match stream.recv_frame().await {
        Ok(Some(f)) => f,
        Ok(None) => {
            debug!("empty stream — ignoring");
            return Ok(());
        }
        Err(e) => {
            debug!(error = %e, "recv_frame failed");
            return Err(e.into());
        }
    };

    debug!(
        type_id = type_id,
        payload_len = payload.len(),
        "received request"
    );

    match type_id {
        // ------------------------------------------------------------------
        // Handshake
        // ------------------------------------------------------------------
        msg::RIFT_HELLO => {
            let hello = RiftHello::decode(payload.as_ref())?;

            if hello.protocol_version != RIFT_PROTOCOL_VERSION {
                anyhow::bail!(
                    "unsupported protocol version {} (expected {})",
                    hello.protocol_version,
                    RIFT_PROTOCOL_VERSION
                );
            }

            // Get or create handle for the share root
            let root_handle = match ctx.handle_db.get_or_create_handle(&ctx.share).await {
                Ok(uuid) => uuid.as_bytes().to_vec(),
                Err(_) => {
                    anyhow::bail!("failed to get root handle for share");
                }
            };

            let welcome = RiftWelcome {
                protocol_version: RIFT_PROTOCOL_VERSION,
                active_capabilities: vec![],
                root_handle,
                max_concurrent_streams: 128,
                share: Some(ShareInfo {
                    name: hello.share_name.clone(),
                    read_only: true, // TODO(v1): derive from permission file
                    cdc_params: None,
                }),
            };

            debug!(share = %hello.share_name, "handshake complete");
            send_welcome(&mut stream, welcome).await?;
        }

        // ------------------------------------------------------------------
        // Metadata operations (with optional Merkle cache)
        // ------------------------------------------------------------------
        msg::STAT_REQUEST => {
            let response =
                handler::stat_response(&payload, &ctx.share, ctx.db(), &ctx.handle_db, ctx.chunker)
                    .await;
            stream
                .send_frame(msg::STAT_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
        }

        msg::LOOKUP_REQUEST => {
            let response = handler::lookup_response(
                &payload,
                &ctx.share,
                ctx.db(),
                &ctx.handle_db,
                ctx.chunker,
            )
            .await;
            stream
                .send_frame(msg::LOOKUP_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
        }

        msg::READDIR_REQUEST => {
            let response = handler::readdir_response(&payload, &ctx.share, &ctx.handle_db).await;
            stream
                .send_frame(msg::READDIR_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
        }

        // ------------------------------------------------------------------
        // Data operations
        // ------------------------------------------------------------------
        msg::READ_REQUEST => {
            handler::read_response(
                &mut stream,
                &payload,
                &ctx.share,
                ctx.db(),
                &ctx.handle_db,
                ctx.chunker,
            )
            .await
            .map_err(|e| anyhow::anyhow!("read failed: {}", e))?;
        }

        msg::MERKLE_DRILL => {
            handler::merkle_drill_response(
                &mut stream,
                &payload,
                &ctx.share,
                ctx.db(),
                &ctx.handle_db,
                ctx.chunker,
            )
            .await
            .map_err(|e| anyhow::anyhow!("merkle_drill failed: {}", e))?;
        }

        // ------------------------------------------------------------------
        // Unknown
        // ------------------------------------------------------------------
        other => {
            debug!("unknown message type 0x{other:02X}");
            let error = ErrorDetail {
                code: ErrorCode::ErrorUnsupported as i32,
                message: format!("unknown message type 0x{other:02X}"),
                metadata: None,
            };
            let _ = stream
                .send_frame(msg::ERROR_RESPONSE, &error.encode_to_vec())
                .await;
            let _ = stream.finish_send().await;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rift_transport::{client_handshake, InMemoryConnector, InMemoryListener, RiftConnection};

    use super::*;
    use crate::{handle::HandleDatabase, handler::merkle_cache_trait::NoopCache};

    /// Helper: build a minimal server config pointing at a real temp directory.
    fn make_share() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // put a token file so the share isn't empty
        std::fs::write(root.join("hello.txt"), b"hello").unwrap();
        (dir, root)
    }

    /// Helper: spin up accept_loop with an InMemoryListener and return the
    /// connector so tests can open connections.
    fn start_in_memory_server(
        share: PathBuf,
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

    // ------------------------------------------------------------------
    // Test 1: accept_loop accepts a connection and performs the handshake
    // ------------------------------------------------------------------
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

    // ------------------------------------------------------------------
    // Test 2: accept_loop exits cleanly when the listener is closed
    // ------------------------------------------------------------------
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
