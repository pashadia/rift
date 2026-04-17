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
use uuid::Uuid;

use rift_common::FsError;
use rift_protocol::messages::{
    lookup_response, msg, read_response, readdir_response, stat_result, BlockHeader, ErrorCode,
    FileAttrs, LookupRequest, LookupResponse, MerkleDrill, MerkleLevelResponse, ReadRequest,
    ReadResponse, ReaddirEntry, ReaddirRequest, ReaddirResponse, StatRequest, StatResponse,
    TransferComplete,
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
/// Construct with [`RiftClient::connect`] for production use, or
/// [`RiftClient::from_connection`] for testing with mock connections.
///
/// Each method opens a new QUIC stream for its operation and closes it when done.
///
/// `RiftClient` is `Clone` (the underlying connection is reference-counted via
/// `Arc`) so it can be shared across `Arc` or moved into tasks.
pub struct RiftClient<C: RiftConnection> {
    conn: C,
    root_handle: Uuid,
    addr: SocketAddr,
    share_name: String,
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl<C: RiftConnection + Clone> Clone for RiftClient<C> {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            root_handle: self.root_handle,
            addr: self.addr,
            share_name: self.share_name.clone(),
            cert: self.cert.clone(),
            key: self.key.clone(),
        }
    }
}

impl RiftClient<QuicConnection> {
    /// Reconnect to the server using stored connection parameters.
    ///
    /// Returns a new client with a fresh connection. The new client preserves
    /// the server address, share name, and TLS certificate from the original.
    pub async fn reconnect(&self) -> Result<Self> {
        tracing::info!(
            addr = %self.addr,
            share_name = %self.share_name,
            "reconnecting to server"
        );

        let ep = client_endpoint(&self.cert, &self.key)?;
        let conn = connect(&ep, self.addr, "rift-server", Arc::new(AcceptAnyPolicy))
            .await
            .map_err(|e| anyhow::anyhow!("QUIC reconnect to {}: {e}", self.addr))?;

        let mut ctrl = conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("open control stream: {e}"))?;

        let welcome = client_handshake(&mut ctrl, &self.share_name, &[])
            .await
            .map_err(|e| anyhow::anyhow!("handshake for share '{}': {e}", self.share_name))?;

        tracing::info!("reconnected successfully");
        let root_handle = Uuid::from_slice(&welcome.root_handle)
            .map_err(|e| anyhow::anyhow!("invalid root handle from server: {e}"))?;
        Ok(Self {
            conn,
            root_handle,
            addr: self.addr,
            share_name: self.share_name.clone(),
            cert: self.cert.clone(),
            key: self.key.clone(),
        })
    }
}

/// Result of a read_chunks operation, containing the fetched chunk data and Merkle root.
pub struct ChunkReadResult {
    pub chunks: Vec<ChunkData>,
    pub merkle_root: Vec<u8>,
}

/// A single chunk's data.
#[derive(Debug)]
pub struct ChunkData {
    pub index: u32,
    pub length: u64,
    pub hash: [u8; 32],
    pub data: Vec<u8>,
}

/// Type alias for a RiftClient backed by a real QUIC connection.
pub type DefaultRiftClient = RiftClient<QuicConnection>;

impl RiftClient<QuicConnection> {
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

        let root_handle = Uuid::from_slice(&welcome.root_handle)
            .map_err(|e| anyhow::anyhow!("invalid root handle from server: {e}"))?;

        Ok(Self {
            conn,
            root_handle,
            addr,
            share_name: share_name.to_string(),
            cert,
            key,
        })
    }

    /// Connect with explicit certificate paths.
    ///
    /// - If `cert_key_paths` is `Some((cert_path, key_path))`, loads the cert/key from those files.
    /// Connect to a Rift server with explicit or persistent certificates.
    ///
    /// - If `cert_key_paths` is provided, loads cert/key from those files.
    /// - Otherwise, loads from `cert_path`/`key_path`, generating and saving
    ///   a persistent self-signed certificate if they don't exist yet.
    #[instrument(fields(addr = %addr, share_name = %share_name), err)]
    pub async fn connect_with_cert(
        addr: SocketAddr,
        share_name: &str,
        cert_key_paths: Option<(&std::path::Path, &std::path::Path)>,
        cert_path: &std::path::Path,
        key_path: &std::path::Path,
    ) -> Result<Self> {
        let (cert, key) = match cert_key_paths {
            Some((cp, kp)) => {
                let cert =
                    std::fs::read(cp).map_err(|e| anyhow::anyhow!("failed to read cert: {e}"))?;
                let key =
                    std::fs::read(kp).map_err(|e| anyhow::anyhow!("failed to read key: {e}"))?;
                (cert, key)
            }
            None => {
                if cert_path.exists() && key_path.exists() {
                    let cert = std::fs::read(cert_path)
                        .map_err(|e| anyhow::anyhow!("failed to read cert: {e}"))?;
                    let key = std::fs::read(key_path)
                        .map_err(|e| anyhow::anyhow!("failed to read key: {e}"))?;
                    (cert, key)
                } else {
                    let (cert, key) = generate_client_cert()?;
                    if let Some(parent) = cert_path.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| anyhow::anyhow!("failed to create state dir: {e}"))?;
                    }
                    std::fs::write(cert_path, &cert)
                        .map_err(|e| anyhow::anyhow!("failed to write cert: {e}"))?;
                    std::fs::write(key_path, &key)
                        .map_err(|e| anyhow::anyhow!("failed to write key: {e}"))?;
                    (cert, key)
                }
            }
        };

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

        let root_handle = Uuid::from_slice(&welcome.root_handle)
            .map_err(|e| anyhow::anyhow!("invalid root handle from server: {e}"))?;

        Ok(Self {
            conn,
            root_handle,
            addr,
            share_name: share_name.to_string(),
            cert,
            key,
        })
    }
}

impl<C: RiftConnection> RiftClient<C> {
    /// Construct a client from an already-established connection and root handle.
    ///
    /// This is primarily useful for testing with mock or recording connections.
    /// For production use, prefer [`RiftClient::connect`] or [`RiftClient::connect_with_cert`].
    pub fn from_connection(conn: C, root_handle: Uuid) -> Self {
        Self {
            conn,
            root_handle,
            addr: "127.0.0.1:0".parse().unwrap(),
            share_name: String::new(),
            cert: Vec::new(),
            key: Vec::new(),
        }
    }

    /// The server certificate fingerprint
    pub fn server_fingerprint(&self) -> &str {
        self.conn.peer_fingerprint()
    }

    /// The opaque root handle for this share, as received in `RiftWelcome`.
    ///
    /// Pass this as the `handle` argument to `stat`, or as `parent` to
    /// `lookup` and `readdir` when operating on the share root.
    pub fn root_handle(&self) -> Uuid {
        self.root_handle
    }

    /// Close the underlying QUIC connection.
    ///
    /// Any in-flight or subsequent operations will return an error promptly.
    /// Primarily useful in tests to simulate connection loss.
    pub fn close_connection(&self) {
        self.conn.close();
    }

    /// Stat multiple handles in batch.
    ///
    /// Returns one result per handle, in the same order as the input.
    /// Each `Ok(attrs)` means the handle exists; `Err(FsError)` indicates
    /// the handle does not exist or is inaccessible.
    ///
    /// Sends a single `StatRequest` with all handles in one network request.
    #[instrument(skip(self), fields(handle_count = handles.len()))]
    pub async fn stat_batch(&self, handles: Vec<Uuid>) -> Result<Vec<Result<FileAttrs, FsError>>> {
        if handles.is_empty() {
            return Ok(Vec::new());
        }

        let handle_bytes: Vec<Vec<u8>> = handles.iter().map(|u| u.as_bytes().to_vec()).collect();

        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("stat_batch: open stream: {e}"))?;

        let req = StatRequest {
            handles: handle_bytes,
        };
        stream
            .send_frame(msg::STAT_REQUEST, &req.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        let (_, payload) = stream
            .recv_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("stat_batch: server closed stream without response"))?;

        let response =
            StatResponse::decode(payload.as_ref()).map_err(|_| anyhow::Error::from(FsError::Io))?;

        let results: Vec<Result<FileAttrs, FsError>> = response
            .results
            .into_iter()
            .map(|result| match result.result {
                Some(stat_result::Result::Attrs(attrs)) => Ok(attrs),
                Some(stat_result::Result::Error(e)) => {
                    let err = map_proto_error(e.code);
                    Err(err.downcast::<FsError>().unwrap_or(FsError::Io))
                }
                None => Err(FsError::Io),
            })
            .collect();

        Ok(results)
    }
}

/// A trait for connection statistics, primarily for testing.
pub trait ConnectionStats {
    fn stream_count(&self) -> usize;
    fn recorded_frames(&self) -> Vec<rift_transport::FrameRecord>;
}

impl ConnectionStats for QuicConnection {
    fn stream_count(&self) -> usize {
        0
    }
    fn recorded_frames(&self) -> Vec<rift_transport::FrameRecord> {
        Vec::new()
    }
}

/// Test helpers for RiftClient backed by RecordingConnection.
/// These are separate from the main impl block because RecordingConnection
/// is specifically designed for testing.
impl RiftClient<rift_transport::RecordingConnection<QuicConnection>> {
    /// Get the number of times `open_stream` was called on the underlying connection.
    pub fn stream_count(&self) -> usize {
        self.conn.stream_count()
    }

    /// Access the frames recorded by the underlying connection.
    pub fn recorded_frames(&self) -> Vec<rift_transport::FrameRecord> {
        self.conn.recorded_frames()
    }
}

impl RiftClient<QuicConnection> {
    // -----------------------------------------------------------------------
    // Async filesystem operations
    // -----------------------------------------------------------------------

    /// Return the attributes of the object identified by `handle`.
    ///
    /// Server `ErrorCode` values are mapped to [`FsError`] variants so the
    /// FUSE layer can produce the correct POSIX errno.
    #[instrument(skip(self))]
    pub async fn stat(&self, handle: Uuid) -> Result<FileAttrs> {
        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("stat: open stream: {e}"))?;

        let req = StatRequest {
            handles: vec![handle.as_bytes().to_vec()],
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
    #[instrument(skip(self), fields(name = %name))]
    pub async fn lookup(&self, parent: Uuid, name: &str) -> Result<(Uuid, FileAttrs)> {
        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("lookup: open stream: {e}"))?;

        let req = LookupRequest {
            parent_handle: parent.as_bytes().to_vec(),
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
                let handle = Uuid::from_slice(&entry.handle)
                    .map_err(|e| anyhow::anyhow!("invalid handle from server: {e}"))?;
                Ok((handle, attrs))
            }
            Some(lookup_response::Result::Error(e)) => Err(map_proto_error(e.code)),
            None => Err(anyhow::Error::from(FsError::Io)),
        }
    }

    /// List the contents of the directory identified by `handle`.
    ///
    /// Returns all entries (no client-side pagination).  FUSE-level
    /// offset/pagination is handled by [`rift_fuse::compute_readdir`].
    #[instrument(skip(self))]
    pub async fn readdir(&self, handle: Uuid) -> Result<Vec<ReaddirEntry>> {
        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("readdir: open stream: {e}"))?;

        let req = ReaddirRequest {
            directory_handle: handle.as_bytes().to_vec(),
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

    // ---------------------------------------------------------------------------
    // Read chunks
    // ---------------------------------------------------------------------------

    /// Read chunks from a file.
    ///
    /// - `handle`: The file handle
    /// - `start_chunk`: First chunk index (0 = from beginning)
    /// - `chunk_count`: Number of chunks to read (0 = all remaining)
    ///
    /// Returns `ChunkReadResult` with chunk data and the file's Merkle root.
    #[instrument(skip(self))]
    pub async fn read_chunks(
        &self,
        handle: Uuid,
        start_chunk: u32,
        chunk_count: u32,
    ) -> Result<ChunkReadResult> {
        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("read_chunks: open stream: {e}"))?;

        let req = ReadRequest {
            handle: handle.as_bytes().to_vec(),
            start_chunk,
            chunk_count,
        };
        stream
            .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        // Read response: ReadSuccess with chunk_count
        let (_, payload) = stream
            .recv_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("read_chunks: server closed stream without response"))?;

        let response =
            ReadResponse::decode(payload.as_ref()).map_err(|_| anyhow::Error::from(FsError::Io))?;

        let chunk_count = match response.result {
            Some(read_response::Result::Ok(success)) => success.chunk_count,
            Some(read_response::Result::Error(e)) => return Err(map_proto_error(e.code)),
            None => return Err(anyhow::Error::from(FsError::Io)),
        };

        // Read each chunk: BLOCK_HEADER + BLOCK_DATA
        let mut chunks = Vec::with_capacity(chunk_count as usize);
        for _i in 0..chunk_count {
            // BlockHeader
            let header_frame = stream
                .recv_frame()
                .await?
                .ok_or_else(|| anyhow::anyhow!("read_chunks: missing BLOCK_HEADER"))?;
            let (_header_type, header_payload) = header_frame;

            let block_header = BlockHeader::decode(header_payload.as_ref())
                .map_err(|_| anyhow::Error::from(FsError::Io))?;
            let chunk_info = block_header
                .chunk
                .ok_or_else(|| anyhow::anyhow!("read_chunks: missing ChunkInfo"))?;

            let index = chunk_info.index;
            let length = chunk_info.length;
            let hash: [u8; 32] = chunk_info
                .hash
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("read_chunks: invalid hash length"))?;

            // BlockData (raw bytes)
            let data_frame = stream
                .recv_frame()
                .await?
                .ok_or_else(|| anyhow::anyhow!("read_chunks: missing BLOCK_DATA"))?;
            let (_data_type, data_payload) = data_frame;

            chunks.push(ChunkData {
                index,
                length,
                hash,
                data: data_payload.to_vec(),
            });
        }

        // TransferComplete with Merkle root
        let root_frame = stream
            .recv_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("read_chunks: missing TRANSFER_COMPLETE"))?;
        let (_root_type, root_payload) = root_frame;

        let transfer_complete =
            TransferComplete::decode(root_payload.as_ref()).map_err(|_| FsError::Io)?;

        Ok(ChunkReadResult {
            chunks,
            merkle_root: transfer_complete.merkle_root,
        })
    }

    // ---------------------------------------------------------------------------
    // MerkleDrill
    // ---------------------------------------------------------------------------

    /// Fetch Merkle tree levels from the server.
    ///
    /// - `handle`: The file handle
    /// - `level`: Which level to fetch (0 = root only)
    /// - `subtrees`: Specific subtree indices to fetch (empty = all at this level)
    ///
    /// Returns `MerkleLevelResponse` with hashes and subtree byte counts.
    #[instrument(skip(self), fields(level, subtrees_len = subtrees.len()))]
    pub async fn merkle_drill(
        &self,
        handle: Uuid,
        level: u32,
        subtrees: &[u32],
    ) -> Result<MerkleLevelResponse> {
        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("merkle_drill: open stream: {e}"))?;

        let req = MerkleDrill {
            handle: handle.as_bytes().to_vec(),
            level,
            subtrees: subtrees.to_vec(),
        };
        stream
            .send_frame(msg::MERKLE_DRILL, &req.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        let (_, payload) = stream.recv_frame().await?.ok_or_else(|| {
            anyhow::anyhow!("merkle_drill: server closed stream without response")
        })?;

        let response = MerkleLevelResponse::decode(payload.as_ref()).map_err(|_| FsError::Io)?;

        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// RemoteShare impl (Linux only)
// ---------------------------------------------------------------------------

/// Wrapper type for MerkleDrill results, simplifying the protocol response.
pub struct MerkleDrillResult {
    pub hashes: Vec<Vec<u8>>,
    pub sizes: Vec<u64>,
}

impl From<MerkleLevelResponse> for MerkleDrillResult {
    fn from(resp: MerkleLevelResponse) -> Self {
        Self {
            hashes: resp.hashes,
            sizes: resp.subtree_bytes,
        }
    }
}

/// Implement `crate::fuse::RemoteShare` so `RiftClient` can be boxed and passed
/// directly to the FUSE mount function.
///
/// The async methods simply delegate to the corresponding `RiftClient` methods.
#[cfg(all(target_os = "linux", feature = "fuse"))]
#[async_trait::async_trait]
impl crate::remote::RemoteShare for RiftClient<QuicConnection> {
    async fn lookup(&self, parent: Uuid, name: &str) -> Result<(Uuid, FileAttrs)> {
        self.lookup(parent, name).await
    }

    async fn readdir(&self, handle: Uuid) -> Result<Vec<ReaddirEntry>> {
        self.readdir(handle).await
    }

    async fn stat_batch(
        &self,
        handles: Vec<Uuid>,
    ) -> anyhow::Result<Vec<Result<FileAttrs, FsError>>> {
        self.stat_batch(handles).await
    }

    async fn read_chunks(
        &self,
        handle: Uuid,
        start_chunk: u32,
        chunk_count: u32,
    ) -> anyhow::Result<ChunkReadResult> {
        self.read_chunks(handle, start_chunk, chunk_count).await
    }

    async fn merkle_drill(
        &self,
        handle: Uuid,
        level: u32,
        subtrees: &[u32],
    ) -> anyhow::Result<MerkleDrillResult> {
        let resp = self.merkle_drill(handle, level, subtrees).await?;
        Ok(resp.into())
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

#[cfg(test)]
mod tests {
    use super::*;
    use rift_common::FsError;

    #[test]
    fn map_proto_error_not_found() {
        let err = map_proto_error(ErrorCode::ErrorNotFound as i32);
        assert!(err.downcast_ref::<FsError>().is_some());
    }

    #[test]
    fn map_proto_error_permission_denied() {
        let err = map_proto_error(ErrorCode::ErrorPermissionDenied as i32);
        let fs_err = err.downcast_ref::<FsError>().unwrap();
        assert!(matches!(fs_err, FsError::PermissionDenied));
    }

    #[test]
    fn map_proto_error_not_a_directory() {
        let err = map_proto_error(ErrorCode::ErrorNotADirectory as i32);
        let fs_err = err.downcast_ref::<FsError>().unwrap();
        assert!(matches!(fs_err, FsError::NotADirectory));
    }

    #[test]
    fn map_proto_error_is_a_directory() {
        let err = map_proto_error(ErrorCode::ErrorIsADirectory as i32);
        let fs_err = err.downcast_ref::<FsError>().unwrap();
        assert!(matches!(fs_err, FsError::NotADirectory));
    }

    #[test]
    fn map_proto_error_unknown_code_maps_to_io() {
        let err = map_proto_error(9999);
        let fs_err = err.downcast_ref::<FsError>().unwrap();
        assert!(matches!(fs_err, FsError::Io));
    }
}
