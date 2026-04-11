//! Rift client: connects to a server, performs handshake, and exposes async
//! filesystem operations (stat, lookup, readdir).
//!
//! # Interface contract
//!
//! Every operation maps to exactly one QUIC stream:
//!
//! ```text
//! client.open_stream()
//!   → send_frame(REQUEST_TYPE, encoded_proto)
//!   → finish_send()
//!   → recv_frame()     ← response frame from server
//!   → decode proto
//!   → return Ok(value) or Err(FsError::*)
//! ```
//!
//! Server-side error codes (`proto::ErrorCode`) are mapped to [`rift_common::FsError`]
//! so the FUSE layer can translate them to the correct POSIX errno.
//!
//! # Sync wrappers
//!
//! [`RiftClient::stat_sync`] / [`readdir_sync`] / [`lookup_sync`] are
//! synchronous wrappers intended for use from `fuser`'s OS threads, which are
//! not tokio worker threads.  They call `self.rt.block_on(...)` where `rt` is a
//! `Handle` captured at `connect()` time from the calling tokio context.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use prost::Message as _;
use tracing::instrument;

use rift_common::FsError;
use rift_protocol::messages::{
    lookup_response, msg, readdir_response, stat_result, ErrorCode, FileAttrs, LookupRequest,
    LookupResponse, ReaddirEntry, ReaddirRequest, ReaddirResponse, StatRequest, StatResponse,
};
use rift_transport::{
    client_endpoint, client_handshake, connect, AcceptAnyPolicy, QuicConnection, RiftConnection,
    RiftStream,
};

// ---------------------------------------------------------------------------
// RiftClient
// ---------------------------------------------------------------------------

/// A connected Rift client session for a single share.
///
/// Construct with [`RiftClient::connect`]; each method opens a new QUIC
/// stream for its operation and closes it when done.
///
/// `RiftClient` is `Clone` (the underlying `QuicConnection` is internally
/// reference-counted) so it can be shared across `Arc` or moved into tasks.
pub struct RiftClient {
    conn: QuicConnection,
    /// Opaque handle for the share root (from `RiftWelcome.root_handle`).
    root_handle: Vec<u8>,
}

impl RiftClient {
    /// Connect to a Rift server, authenticate, and mount `share_name`.
    ///
    /// Generates an ephemeral self-signed TLS certificate for this session.
    ///
    /// # TODO
    /// - `TODO(v1)`: load a persistent cert from `~/.config/rift/client.{cert,key}`
    ///   so the client fingerprint is stable across reconnects and the server
    ///   admin's permission files continue to authorise the client.
    /// - `TODO(v1)`: use [`rift_transport::TofuPolicy`] loaded from
    ///   `~/.config/rift/known-servers.toml` instead of [`AcceptAnyPolicy`].
    #[instrument(fields(addr = %addr, share_name = %share_name), err)]
    pub async fn connect(addr: SocketAddr, share_name: &str) -> Result<Self> {
        let (cert, key) = generate_client_cert()?;
        let ep = client_endpoint(&cert, &key)?;

        let conn = connect(&ep, addr, "rift-server", Arc::new(AcceptAnyPolicy))
            .await
            .map_err(|e| anyhow::anyhow!("QUIC connect to {addr}: {e}"))?;

        let mut ctrl = conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("open control stream: {e}"))?;

        let welcome = client_handshake(&mut ctrl, share_name, &[])
            .await
            .map_err(|e| anyhow::anyhow!("handshake for share '{share_name}': {e}"))?;

        Ok(Self {
            conn,
            root_handle: welcome.root_handle,
        })
    }

    /// The server certificate fingerprint
    pub fn server_fingerprint(&self) -> &str {
        self.conn.peer_fingerprint()
    }

    /// The opaque root handle for this share, as received in `RiftWelcome`.
    ///
    /// Pass this as the `handle` argument to `stat`, or as `parent` to
    /// `lookup` and `readdir` when operating on the share root.
    pub fn root_handle(&self) -> &[u8] {
        &self.root_handle
    }

    /// Close the underlying QUIC connection.
    ///
    /// Any in-flight or subsequent operations will return an error promptly.
    /// Primarily useful in tests to simulate connection loss.
    pub fn close_connection(&self) {
        self.conn.close();
    }

    // -----------------------------------------------------------------------
    // Async filesystem operations
    // -----------------------------------------------------------------------

    /// Return the attributes of the object identified by `handle`.
    ///
    /// Server `ErrorCode` values are mapped to [`FsError`] variants so the
    /// FUSE layer can produce the correct POSIX errno.
    #[instrument(skip(self), fields(handle_len = handle.len()), err)]
    pub async fn stat(&self, handle: &[u8]) -> Result<FileAttrs> {
        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("stat: open stream: {e}"))?;

        let req = StatRequest {
            handles: vec![handle.to_vec()],
        };
        stream
            .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        let (_, payload) = stream
            .recv_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("stat: server closed stream without response"))?;

        let response =
            StatResponse::decode(payload.as_ref()).map_err(|_| anyhow::Error::from(FsError::Io))?;

        let result = response
            .results
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::Error::from(FsError::Io))?;

        match result.result {
            Some(stat_result::Result::Attrs(attrs)) => Ok(attrs),
            Some(stat_result::Result::Error(e)) => Err(map_proto_error(e.code)),
            None => Err(anyhow::Error::from(FsError::Io)),
        }
    }

    /// Resolve `name` within the directory identified by `parent`.
    ///
    /// Returns `(child_handle, child_attrs)`.
    #[instrument(skip(self), fields(parent_len = parent.len(), name = %name), err)]
    pub async fn lookup(&self, parent: &[u8], name: &str) -> Result<(Vec<u8>, FileAttrs)> {
        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("lookup: open stream: {e}"))?;

        let req = LookupRequest {
            parent_handle: parent.to_vec(),
            name: name.to_string(),
        };
        stream
            .send_frame(msg::LOOKUP_REQUEST, &req.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        let (_, payload) = stream
            .recv_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("lookup: server closed stream without response"))?;

        let response = LookupResponse::decode(payload.as_ref())
            .map_err(|_| anyhow::Error::from(FsError::Io))?;

        match response.result {
            Some(lookup_response::Result::Entry(entry)) => {
                let attrs = entry
                    .attrs
                    .ok_or_else(|| anyhow::Error::from(FsError::Io))?;
                Ok((entry.handle, attrs))
            }
            Some(lookup_response::Result::Error(e)) => Err(map_proto_error(e.code)),
            None => Err(anyhow::Error::from(FsError::Io)),
        }
    }

    /// List the contents of the directory identified by `handle`.
    ///
    /// Returns all entries (no client-side pagination).  FUSE-level
    /// offset/pagination is handled by [`rift_fuse::compute_readdir`].
    #[instrument(skip(self), fields(handle_len = handle.len()), err)]
    pub async fn readdir(&self, handle: &[u8]) -> Result<Vec<ReaddirEntry>> {
        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("readdir: open stream: {e}"))?;

        let req = ReaddirRequest {
            directory_handle: handle.to_vec(),
            offset: 0,
            limit: 0, // 0 = return all entries
        };
        stream
            .send_frame(msg::READDIR_REQUEST, &req.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        let (_, payload) = stream
            .recv_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("readdir: server closed stream without response"))?;

        let response = ReaddirResponse::decode(payload.as_ref())
            .map_err(|_| anyhow::Error::from(FsError::Io))?;

        match response.result {
            Some(readdir_response::Result::Entries(success)) => Ok(success.entries),
            Some(readdir_response::Result::Error(e)) => Err(map_proto_error(e.code)),
            None => Err(anyhow::Error::from(FsError::Io)),
        }
    }
}

// ---------------------------------------------------------------------------
// RemoteShare impl (Linux only)
// ---------------------------------------------------------------------------

/// Implement `crate::fuse::RemoteShare` so `RiftClient` can be boxed and passed
/// directly to the FUSE mount function.
///
/// The async methods simply delegate to the corresponding `RiftClient` methods.
#[cfg(all(target_os = "linux", feature = "fuse"))]
#[async_trait::async_trait]
impl crate::remote::RemoteShare for RiftClient {
    async fn lookup(&self, parent: &[u8], name: &str) -> Result<(Vec<u8>, FileAttrs)> {
        self.lookup(parent, name).await
    }

    async fn readdir(&self, handle: &[u8]) -> Result<Vec<ReaddirEntry>> {
        self.readdir(handle).await
    }

    async fn stat_batch(
        &self,
        handles: Vec<Vec<u8>>,
    ) -> anyhow::Result<Vec<Result<FileAttrs, FsError>>> {
        // This is a temporary implementation that makes N calls.
        // TODO: Implement a real batch STAT message in the protocol.
        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            let result = self.stat(&handle).await;
            match result {
                Ok(attrs) => results.push(Ok(attrs)),
                Err(e) => {
                    let fs_error = e.downcast::<FsError>().unwrap_or(FsError::Io);
                    results.push(Err(fs_error));
                }
            }
        }
        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Map a proto `ErrorCode` (i32 discriminant) to an `anyhow::Error` wrapping
/// the appropriate [`FsError`] variant.
///
/// Any code without a direct mapping becomes `FsError::Io` (→ `EIO`), which
/// ensures the FUSE layer always has a valid errno even for future error codes
/// it doesn't know about.
fn map_proto_error(code: i32) -> anyhow::Error {
    let fs_err = match ErrorCode::try_from(code) {
        Ok(ErrorCode::ErrorNotFound) => FsError::NotFound,
        Ok(ErrorCode::ErrorPermissionDenied) => FsError::PermissionDenied,
        Ok(ErrorCode::ErrorNotADirectory) => FsError::NotADirectory,
        Ok(ErrorCode::ErrorIsADirectory) => FsError::NotADirectory,
        _ => FsError::Io,
    };
    anyhow::Error::from(fs_err)
}

// ---------------------------------------------------------------------------
// Certificate generation
// ---------------------------------------------------------------------------

/// Generate an ephemeral self-signed TLS certificate for this client session.
///
/// # TODO(v1)
/// Load a persistent certificate from `~/.config/rift/client.{cert,key}` so
/// the client's BLAKE3 fingerprint is stable across restarts.  The server's
/// per-share permission files authorise clients by fingerprint, so an
/// ephemeral cert forces re-authorisation after every restart.
fn generate_client_cert() -> Result<(Vec<u8>, Vec<u8>)> {
    let cert = rcgen::generate_simple_self_signed(vec!["rift-client".to_string()])?;
    Ok((cert.cert.der().to_vec(), cert.key_pair.serialize_der()))
}
