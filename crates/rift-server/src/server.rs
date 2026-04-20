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

use rift_protocol::messages::{
    msg, discover_response, DiscoverRequest, DiscoverResponse, RiftHello, RiftWelcome, ShareInfo,
    WhoamiRequest, WhoamiResponse,
};
use rift_transport::{
    send_welcome, QuicConnection, QuicListener, QuicStream, RiftConnection, RiftListener,
    RiftStream, TransportError, RIFT_PROTOCOL_VERSION,
};

use crate::{handle::HandleDatabase, handler, metadata::db::Database};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Accept connections in a loop and serve each one in a background task.
///
/// Uses a Merkle cache database for computing root hashes. Pass `None` for `db`
/// to disable caching (root hashes will still be computed on-demand).
#[instrument(skip(listener, db, handle_db), fields(share = %share.display(), listen_addr = %listener.local_addr()))]
pub async fn accept_loop(
    listener: QuicListener,
    share: PathBuf,
    db: Arc<Option<Database>>,
    handle_db: Arc<HandleDatabase>,
) -> anyhow::Result<()> {
    loop {
        match listener.accept().await {
            Ok(conn) => {
                let share = share.clone();
                let db = db.clone();
                let handle_db = handle_db.clone();
                let peer = conn.peer_fingerprint().to_string();
                let conn_span = debug_span!("server.connection", peer = %peer);
                tokio::spawn(
                    async move {
                        serve_connection(conn, share, db, handle_db).await;
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

#[instrument(skip(conn, share, db, handle_db), fields(share = %share.display(), peer = %conn.peer_fingerprint()))]
async fn serve_connection(
    conn: QuicConnection,
    share: PathBuf,
    db: Arc<Option<Database>>,
    handle_db: Arc<HandleDatabase>,
) {
    debug!("connection established");

    // TODO(v1): authorise peer fingerprint against per-share permission files
    // before accepting any streams.

    loop {
        match conn.accept_stream().await {
            Ok(stream) => {
                let share = share.clone();
                let db = db.clone();
                let handle_db = handle_db.clone();
                let stream_span = debug_span!("server.stream", share = %share.display());
                tokio::spawn(
                    async move {
                        if let Err(e) = handle_stream(stream, share, db, handle_db).await {
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

#[instrument(skip_all, fields(share = %share.display()), err)]
async fn handle_stream(
    mut stream: QuicStream,
    share: PathBuf,
    db: Arc<Option<Database>>,
    handle_db: Arc<HandleDatabase>,
) -> anyhow::Result<()> {
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
            let root_handle = match handle_db.get_or_create_handle(&share).await {
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

        msg::WHOAMI_REQUEST => {
            let _req = WhoamiRequest::decode(payload.as_ref())?;
            let response = WhoamiResponse {
                fingerprint: "test-fingerprint".to_string(),
                common_name: "test-client".to_string(),
                available_shares: vec![],
            };
            stream
                .send_frame(msg::WHOAMI_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
        }

        msg::DISCOVER_REQUEST => {
            let _req = DiscoverRequest::decode(payload.as_ref())?;
            let response = DiscoverResponse {
                shares: vec![discover_response::Share {
                    name: "demo".to_string(),
                    description: "Demo share".to_string(),
                    read_only: true,
                    is_public: true,
                }],
            };
            stream
                .send_frame(msg::DISCOVER_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
        }

        // ------------------------------------------------------------------
        // Metadata operations (with optional Merkle cache)
        // ------------------------------------------------------------------
        msg::STAT_REQUEST => {
            let db_ref = db.as_ref().as_ref();
            let response = handler::stat_response(&payload, &share, db_ref, &handle_db).await;
            stream
                .send_frame(msg::STAT_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
        }

        msg::LOOKUP_REQUEST => {
            let db_ref = db.as_ref().as_ref();
            let response = handler::lookup_response(&payload, &share, db_ref, &handle_db).await;
            stream
                .send_frame(msg::LOOKUP_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
        }

        msg::READDIR_REQUEST => {
            let response = handler::readdir_response(&payload, &share, &handle_db).await;
            stream
                .send_frame(msg::READDIR_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
        }

        msg::MKDIR_REQUEST => {
            let response = handler::mkdir_response(&payload, &share, &handle_db).await;
            stream
                .send_frame(msg::MKDIR_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
        }

        // ------------------------------------------------------------------
        // Data operations
        // ------------------------------------------------------------------
        msg::READ_REQUEST => {
            let db_ref = db.as_ref().as_ref();
            handler::read_response(&mut stream, &payload, &share, db_ref, &handle_db)
                .await
                .map_err(|e| anyhow::anyhow!("read failed: {}", e))?;
        }

        msg::MERKLE_DRILL => {
            let db_ref = db.as_ref().as_ref();
            handler::merkle_drill_response(&mut stream, &payload, &share, db_ref, &handle_db)
                .await
                .map_err(|e| anyhow::anyhow!("merkle_drill failed: {}", e))?;
        }

        // ------------------------------------------------------------------
        // Unknown
        // ------------------------------------------------------------------
        other => {
            // TODO(v1): send ERROR_RESPONSE so clients surface a real error.
            debug!("unknown message type 0x{other:02X} — closing stream");
        }
    }

    Ok(())
}
