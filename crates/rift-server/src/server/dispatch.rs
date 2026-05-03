//! Per-connection and per-stream dispatch.
//!
//! This module contains the request routing logic: the connection loop
//! (`serve_connection`), the stream handler (`handle_stream`), and all
//! the individual dispatch helpers.

use prost::Message as _;
use tracing::{debug, instrument, Instrument};

use rift_protocol::messages::{msg, ErrorCode, ErrorDetail, RiftHello, RiftWelcome, ShareInfo};
use rift_transport::{send_welcome, RiftConnection, RiftStream, RIFT_PROTOCOL_VERSION};

use crate::handler;
use crate::handler::merkle_cache_trait::MerkleCache;
use crate::server::context::{recv_request, IncomingRequest, RequestContext};

// ---------------------------------------------------------------------------
// Per-connection loop
// ---------------------------------------------------------------------------

#[instrument(skip(conn, ctx), fields(share = %ctx.share.display(), peer = %conn.peer_fingerprint()))]
pub(crate) async fn serve_connection<C, M: MerkleCache + 'static>(conn: C, ctx: RequestContext<M>)
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
                let stream_span =
                    tracing::debug_span!("server.stream", share = %ctx.share.display());
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
    let Some(request) = recv_request(&mut stream).await? else {
        return Ok(());
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

/// Handle a `RIFT_HELLO` handshake on the given stream.
///
/// Validates protocol version, creates/gets the root handle, and sends
/// `RIFT_WELCOME`. Returns `Ok(())` on success, `Err` on version mismatch
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

/// Dispatch a simple request→response: call `handler`, encode the response,
/// send a single frame, then `finish_send`.
///
/// This eliminates the repeated "call handler → encode → `send_frame` → `finish_send`"
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

/// Handle a `READ_REQUEST` by delegating to the read handler with context.
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

/// Handle a `MERKLE_DRILL` request by delegating to the drill handler with context.
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use prost::Message;
    use rift_common::crypto::Chunker;
    use rift_protocol::messages::{msg, ErrorDetail, ReadRequest, RiftHello, RiftWelcome};
    use rift_transport::connection::InMemoryConnection;
    use rift_transport::{RiftConnection, RIFT_PROTOCOL_VERSION};

    use super::*;
    use crate::handle::HandleDatabase;
    use crate::handler::merkle_cache_trait::NoopCache;

    /// Helper: build a minimal share directory with a test file.
    fn make_share() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("hello.txt"), b"hello").unwrap();
        (dir, root)
    }

    #[tokio::test]
    async fn handle_handshake_with_valid_hello_succeeds() {
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

        let result = handle_handshake(&mut server_stream, &ctx, hello).await;
        assert!(result.is_ok(), "handshake should succeed");

        let (type_id, payload) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::RIFT_WELCOME);
        let welcome = RiftWelcome::decode(payload.as_ref()).unwrap();
        assert_eq!(welcome.protocol_version, RIFT_PROTOCOL_VERSION);
        assert_eq!(welcome.root_handle.len(), 16);
    }

    #[tokio::test]
    async fn handle_handshake_rejects_wrong_protocol_version() {
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
            protocol_version: 0xFF,
            share_name: "test-share".to_string(),
            capabilities: vec![],
        };

        let result = handle_handshake(&mut server_stream, &ctx, hello).await;
        assert!(result.is_err(), "handshake should fail with wrong version");
    }

    #[tokio::test]
    async fn dispatch_simple_sends_frame_and_finishes() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let result = dispatch_simple(&mut server_stream, msg::STAT_RESPONSE, || async {
            rift_protocol::messages::StatResponse::default()
        })
        .await;
        assert!(result.is_ok(), "dispatch_simple should succeed");

        let (type_id, _payload) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::STAT_RESPONSE);
        assert!(client_stream.recv_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn handle_read_request_proxies_to_handler() {
        let (_dir, share) = make_share();
        let db: Arc<NoopCache> = Arc::new(NoopCache);
        let handle_db = Arc::new(HandleDatabase::new());

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

        let (type_id, _resp_payload) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::READ_RESPONSE);
    }

    #[tokio::test]
    async fn send_error_response_sends_error_frame() {
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

        let (type_id, payload) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::ERROR_RESPONSE);
        let error = ErrorDetail::decode(payload.as_ref()).unwrap();
        assert_eq!(error.code, ErrorCode::ErrorUnsupported as i32);
        assert_eq!(error.message, "test error");
    }
}
