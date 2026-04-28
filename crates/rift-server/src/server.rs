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

/// The first frame received on a new stream.
struct IncomingRequest {
    type_id: u8,
    payload: bytes::Bytes,
}

/// Receive the first frame from a stream, or return Ok(None) for empty streams.
async fn recv_request<S: RiftStream>(stream: &mut S) -> anyhow::Result<Option<IncomingRequest>> {
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

#[instrument(skip_all, fields(share = %ctx.share.display()), err)]
async fn handle_stream<S, M: MerkleCache>(
    mut stream: S,
    ctx: RequestContext<M>,
) -> anyhow::Result<()>
where
    S: RiftStream,
{
    let request = match recv_request(&mut stream).await? {
        Some(r) => r,
        None => return Ok(()),
    };

    debug!(
        type_id = request.type_id,
        payload_len = request.payload.len(),
        "received request"
    );

    dispatch_request(&mut stream, request, &ctx).await
}

/// Dispatch an incoming request to the appropriate handler.
#[instrument(skip_all, fields(share = %ctx.share.display()), err)]
async fn dispatch_request<S, M: MerkleCache>(
    stream: &mut S,
    request: IncomingRequest,
    ctx: &RequestContext<M>,
) -> anyhow::Result<()>
where
    S: RiftStream,
{
    match request.type_id {
        // ------------------------------------------------------------------
        // Handshake
        // ------------------------------------------------------------------
        msg::RIFT_HELLO => {
            let hello = RiftHello::decode(request.payload.as_ref())?;
            handle_handshake(stream, ctx, hello).await?;
        }

        // ------------------------------------------------------------------
        // Metadata operations (with optional Merkle cache)
        // ------------------------------------------------------------------
        msg::STAT_REQUEST => {
            dispatch_simple(stream, msg::STAT_RESPONSE, || {
                handler::stat_response(
                    &request.payload,
                    &ctx.share,
                    ctx.db(),
                    &ctx.handle_db,
                    ctx.chunker,
                )
            })
            .await?;
        }

        msg::LOOKUP_REQUEST => {
            dispatch_simple(stream, msg::LOOKUP_RESPONSE, || {
                handler::lookup_response(
                    &request.payload,
                    &ctx.share,
                    ctx.db(),
                    &ctx.handle_db,
                    ctx.chunker,
                )
            })
            .await?;
        }

        msg::READDIR_REQUEST => {
            dispatch_simple(stream, msg::READDIR_RESPONSE, || {
                handler::readdir_response(&request.payload, &ctx.share, &ctx.handle_db)
            })
            .await?;
        }

        // ------------------------------------------------------------------
        // Data operations
        // ------------------------------------------------------------------
        msg::READ_REQUEST => {
            handle_read_request(stream, &request.payload, ctx).await?;
        }

        msg::MERKLE_DRILL => {
            handle_merkle_drill_request(stream, &request.payload, ctx).await?;
        }

        // ------------------------------------------------------------------
        // Unknown
        // ------------------------------------------------------------------
        other => {
            debug!("unknown message type 0x{other:02X}");
            // Best-effort: don't fail the stream if error send fails
            let _ = send_error_response(
                stream,
                ErrorCode::ErrorUnsupported,
                format!("unknown message type 0x{other:02X}"),
            )
            .await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handshake handler
// ---------------------------------------------------------------------------

/// Handle a RIFT_HELLO handshake on the given stream.
///
/// Validates protocol version, creates/gets the root handle, and sends
/// RIFT_WELCOME. Returns `Ok(())` on success, `Err` on version mismatch
/// or stream failure.
async fn handle_handshake<S: RiftStream, M: MerkleCache>(
    stream: &mut S,
    ctx: &RequestContext<M>,
    hello: RiftHello,
) -> anyhow::Result<()> {
    if hello.protocol_version != RIFT_PROTOCOL_VERSION {
        anyhow::bail!(
            "unsupported protocol version {} (expected {})",
            hello.protocol_version,
            RIFT_PROTOCOL_VERSION
        );
    }

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
    send_welcome(stream, welcome).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Simple request→response dispatch
// ---------------------------------------------------------------------------

/// Dispatch a simple request<>response: call `handler`, encode the response,
/// send a single frame, then `finish_send`.
///
/// This eliminates the repeated "call handler → encode → send_frame → finish_send"
/// pattern for stat, lookup, and readdir handlers.
async fn dispatch_simple<S, F, Fut, R>(
    stream: &mut S,
    response_type: u8,
    handler: F,
) -> anyhow::Result<()>
where
    S: RiftStream,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
    R: prost::Message,
{
    let response = handler().await;
    stream
        .send_frame(response_type, &response.encode_to_vec())
        .await?;
    stream.finish_send().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Error response helper
// ---------------------------------------------------------------------------

/// Send an error response frame on the stream.
///
/// Constructs an `ErrorDetail` message and sends it as a single frame.
/// Returns `Err` if the send itself fails, but callers may use `let _ = `
/// to ignore send failures (e.g. when the client may have disconnected).
async fn send_error_response<S: RiftStream>(
    stream: &mut S,
    code: ErrorCode,
    message: String,
) -> anyhow::Result<()> {
    let error = ErrorDetail {
        code: code as i32,
        message,
        metadata: None,
    };
    stream
        .send_frame(msg::ERROR_RESPONSE, &error.encode_to_vec())
        .await?;
    stream.finish_send().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Streaming dispatch wrappers
// ---------------------------------------------------------------------------

/// Handle a READ_REQUEST by delegating to the read handler with context.
async fn handle_read_request<S: RiftStream, M: MerkleCache>(
    stream: &mut S,
    payload: &[u8],
    ctx: &RequestContext<M>,
) -> anyhow::Result<()> {
    handler::read_response(
        stream,
        payload,
        &ctx.share,
        ctx.db(),
        &ctx.handle_db,
        ctx.chunker,
    )
    .await
    .map_err(|e| anyhow::anyhow!("read failed: {}", e))
}

/// Handle a MERKLE_DRILL request by delegating to the drill handler with context.
async fn handle_merkle_drill_request<S: RiftStream, M: MerkleCache>(
    stream: &mut S,
    payload: &[u8],
    ctx: &RequestContext<M>,
) -> anyhow::Result<()> {
    handler::merkle_drill_response(
        stream,
        payload,
        &ctx.share,
        ctx.db(),
        &ctx.handle_db,
        ctx.chunker,
    )
    .await
    .map_err(|e| anyhow::anyhow!("merkle_drill failed: {}", e))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use prost::Message;
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
    // Test 3: handle_handshake with valid hello succeeds
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn handle_handshake_with_valid_hello_succeeds() {
        use rift_transport::connection::InMemoryConnection;

        let (_dir, share) = make_share();
        let db: Arc<NoopCache> = Arc::new(NoopCache);
        let handle_db = Arc::new(HandleDatabase::new());
        let ctx = RequestContext {
            share,
            db,
            handle_db,
            chunker: Chunker::default(),
        };

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let hello = RiftHello {
            protocol_version: RIFT_PROTOCOL_VERSION,
            share_name: "test-share".to_string(),
            capabilities: vec![],
        };

        // Call handle_handshake directly
        let result = handle_handshake(&mut server_stream, &ctx, hello).await;
        assert!(result.is_ok(), "handshake should succeed");

        // Client should receive a RIFT_WELCOME frame
        let (type_id, payload) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::RIFT_WELCOME);
        let welcome = RiftWelcome::decode(payload.as_ref()).unwrap();
        assert_eq!(welcome.protocol_version, RIFT_PROTOCOL_VERSION);
        assert_eq!(welcome.root_handle.len(), 16); // UUID bytes
    }

    // ------------------------------------------------------------------
    // Test 4: handle_handshake rejects wrong protocol version
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn handle_handshake_rejects_wrong_protocol_version() {
        use rift_transport::connection::InMemoryConnection;

        let (_dir, share) = make_share();
        let db: Arc<NoopCache> = Arc::new(NoopCache);
        let handle_db = Arc::new(HandleDatabase::new());
        let ctx = RequestContext {
            share,
            db,
            handle_db,
            chunker: Chunker::default(),
        };

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut _client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let hello = RiftHello {
            protocol_version: 0xFF, // Wrong version
            share_name: "test-share".to_string(),
            capabilities: vec![],
        };

        let result = handle_handshake(&mut server_stream, &ctx, hello).await;
        assert!(result.is_err(), "handshake should fail with wrong version");
    }

    // ------------------------------------------------------------------
    // Test 5: dispatch_simple sends a single frame and finishes
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn dispatch_simple_sends_frame_and_finishes() {
        use rift_transport::connection::InMemoryConnection;

        let (_dir, share) = make_share();
        let db: Arc<NoopCache> = Arc::new(NoopCache);
        let handle_db = Arc::new(HandleDatabase::new());
        // dispatch_simple doesn't use ctx directly in this unit test,
        // since we provide a simple closure that returns a default response.
        let _ctx = RequestContext {
            share,
            db,
            handle_db,
            chunker: Chunker::default(),
        };

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        // Use dispatch_simple to send a minimal response
        let result = dispatch_simple(&mut server_stream, msg::STAT_RESPONSE, || async {
            rift_protocol::messages::StatResponse::default()
        })
        .await;
        assert!(result.is_ok(), "dispatch_simple should succeed");

        // Client should receive exactly one frame
        let (type_id, _payload) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::STAT_RESPONSE);
        // After finish_send, client should get Ok(None)
        assert!(client_stream.recv_frame().await.unwrap().is_none());
    }

    // ------------------------------------------------------------------
    // Test 6: handle_read_request proxies to handler::read_response
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn handle_read_request_proxies_to_handler() {
        use rift_protocol::messages::ReadRequest;
        use rift_transport::connection::InMemoryConnection;

        let (_dir, share) = make_share();
        let db: Arc<NoopCache> = Arc::new(NoopCache);
        let handle_db = Arc::new(HandleDatabase::new());

        // Register a handle for hello.txt
        let file_path = share.join("hello.txt");
        let uuid = handle_db.get_or_create_handle(&file_path).await.unwrap();

        let ctx = RequestContext {
            share,
            db,
            handle_db,
            chunker: Chunker::default(),
        };

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let req = ReadRequest {
            handle: uuid.as_bytes().to_vec(),
            start_chunk: 0,
            chunk_count: 1,
        };
        let payload = req.encode_to_vec();

        let result = handle_read_request(&mut server_stream, &payload, &ctx).await;
        assert!(
            result.is_ok(),
            "handle_read_request should succeed: {:?}",
            result
        );

        // Client should receive at least one response frame
        let (type_id, _resp_payload) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::READ_RESPONSE);
    }

    // ------------------------------------------------------------------
    // Test 7: send_error_response sends an error frame
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn send_error_response_sends_error_frame() {
        use rift_transport::connection::InMemoryConnection;

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let result = send_error_response(
            &mut server_stream,
            ErrorCode::ErrorUnsupported,
            "test error".to_string(),
        )
        .await;
        assert!(result.is_ok(), "send_error_response should succeed");

        // Client should receive an ERROR_RESPONSE frame
        let (type_id, payload) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::ERROR_RESPONSE);
        let error = ErrorDetail::decode(payload.as_ref()).unwrap();
        assert_eq!(error.code, ErrorCode::ErrorUnsupported as i32);
        assert_eq!(error.message, "test error");
    }

    // ------------------------------------------------------------------
    // Test 8: recv_request returns Some for a valid frame
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn recv_request_returns_some_for_valid_frame() {
        use rift_transport::connection::InMemoryConnection;

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

    // ------------------------------------------------------------------
    // Test 9: recv_request returns None for an empty stream
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn recv_request_returns_none_for_empty_stream() {
        use rift_transport::connection::InMemoryConnection;

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
