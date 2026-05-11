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
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use prost::Message as _;
use tracing::instrument;
use uuid::Uuid;
use zeroize::Zeroizing;

use rift_common::FsError;
use rift_protocol::messages::{
    lookup_response, msg, read_response, readdir_response, stat_result, BlockHeader, ErrorCode,
    FileAttrs, LookupRequest, LookupResponse, MerkleChildType, MerkleDrill, MerkleDrillResponse,
    ReadRequest, ReadResponse, ReaddirEntry, ReaddirRequest, ReaddirResponse, ShareInfo,
    StatRequest, StatResponse, TransferComplete, WhoamiRequest, WhoamiResponse,
};
use rift_transport::{
    client_endpoint, client_handshake, connect, AcceptAnyPolicy, QuicConnection, RiftConnection,
    RiftStream, TofuPolicy, TofuStore,
};

use crate::paths::ClientPaths;

struct TofuState {
    store: Arc<std::sync::Mutex<TofuStore>>,
    path: PathBuf,
}

impl TofuState {
    fn new(store: Arc<std::sync::Mutex<TofuStore>>, path: PathBuf) -> Self {
        Self { store, path }
    }

    fn save_if_dirty(&self) -> Result<()> {
        let snapshot = {
            let store = self
                .store
                .lock()
                .map_err(|e| anyhow::anyhow!("mutex poisoned: {}", e))?;
            if !store.dirty {
                return Ok(());
            }
            TofuStore::new(store.known.clone())
        };
        crate::known_servers::save_known_servers(&self.path, &snapshot)
    }
}

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
    /// Private key bytes — zeroed on drop to prevent key material leakage
    /// in swap files, core dumps, or ptrace reads.
    key: Zeroizing<Vec<u8>>,
    tofu_state: Option<TofuState>,
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
            tofu_state: self.tofu_state.clone(),
        }
    }
}

impl Clone for TofuState {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            path: self.path.clone(),
        }
    }
}

impl RiftClient<QuicConnection> {
    /// Reconnect to the server using stored connection parameters.
    ///
    /// Returns a new client with a fresh connection. The new client preserves
    /// the server address, share name, and TLS certificate from the original.
    /// If TOFU state is present, uses `TofuPolicy`; otherwise `AcceptAnyPolicy`.
    pub async fn reconnect(&self) -> Result<Self> {
        tracing::info!(
            addr = %self.addr,
            share_name = %self.share_name,
            "reconnecting to server"
        );

        let ep = client_endpoint(&self.cert, &self.key)?;

        let conn = if let Some(ref tofu) = self.tofu_state {
            let known = {
                let store = tofu
                    .store
                    .lock()
                    .map_err(|e| anyhow::anyhow!("mutex poisoned: {}", e))?;
                store.known.clone()
            };
            let policy = TofuPolicy::new(format!("{}", self.addr), known);
            let store_arc = policy.store();
            let conn = connect(&ep, self.addr, "rift-server", Arc::new(policy))
                .await
                .map_err(|e| anyhow::anyhow!("QUIC reconnect to {}: {e}", self.addr))?;

            {
                let mut original = tofu
                    .store
                    .lock()
                    .map_err(|e| anyhow::anyhow!("mutex poisoned: {}", e))?;
                let updated = store_arc
                    .lock()
                    .map_err(|e| anyhow::anyhow!("mutex poisoned: {}", e))?;
                original.known = updated.known.clone();
                if updated.dirty {
                    original.dirty = true;
                }
            }

            conn
        } else {
            connect(&ep, self.addr, "rift-server", Arc::new(AcceptAnyPolicy))
                .await
                .map_err(|e| anyhow::anyhow!("QUIC reconnect to {}: {e}", self.addr))?
        };

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

        let new_client = Self {
            conn,
            root_handle,
            addr: self.addr,
            share_name: self.share_name.clone(),
            cert: self.cert.clone(),
            key: self.key.clone(),
            tofu_state: self.tofu_state.clone(),
        };

        if let Some(ref tofu) = new_client.tofu_state {
            tofu.save_if_dirty()?;
        }

        Ok(new_client)
    }
}

/// Result of a `read_chunks` operation, containing the fetched chunk data and Merkle root.
#[derive(Debug, Clone)]
pub struct ChunkReadResult {
    pub chunks: Vec<ChunkData>,
    pub merkle_root: Vec<u8>,
}

impl ChunkReadResult {
    /// Assert exactly one chunk was returned and return it.
    ///
    /// This is a convenience helper for the single-chunk fetch path.
    #[must_use]
    pub fn single(self) -> ChunkData {
        assert_eq!(
            self.chunks.len(),
            1,
            "expected exactly one chunk, got {}",
            self.chunks.len()
        );
        self.chunks
            .into_iter()
            .next()
            .expect("len() == 1 guarantee")
    }
}

/// A single chunk's data.
#[derive(Debug, Clone)]
pub struct ChunkData {
    pub index: u32,
    pub length: u64,
    pub hash: [u8; 32],
    pub data: Bytes,
}

/// Type alias for a RiftClient backed by a real QUIC connection.
pub type DefaultRiftClient = RiftClient<QuicConnection>;

impl RiftClient<QuicConnection> {
    /// Connect to a Rift server with an ephemeral certificate and no TOFU.
    ///
    /// Suitable for testing only. For production use, prefer
    /// [`RiftClient::connect_persistent`] which provides a stable fingerprint
    /// and Trust-On-First-Use server verification.
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
            key: Zeroizing::new(key),
            tofu_state: None,
        })
    }

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
                    write_private_key(key_path, &key)
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
            key: Zeroizing::new(key),
            tofu_state: None,
        })
    }

    /// Connect with persistent client certificate and TOFU policy.
    ///
    /// Uses [`ClientPaths`] to resolve cert/key and known-servers.toml paths.
    /// - Loads or generates a persistent client certificate (stable fingerprint).
    /// - Uses [`TofuPolicy`] loaded from `known-servers.toml` for server verification.
    /// - Saves TOFU state after successful connection.
    ///
    /// If `cert_key_paths` is provided, overrides the default cert/key paths
    /// (useful for `--cert`/`--key` CLI flags).
    #[instrument(fields(addr = %addr, share_name = %share_name), err)]
    pub async fn connect_persistent(
        addr: SocketAddr,
        share_name: &str,
        paths: &ClientPaths,
    ) -> Result<Self> {
        let cert_path = paths.cert_path();
        let key_path = paths.key_path();

        let (cert, key) = if cert_path.exists() && key_path.exists() {
            let cert = std::fs::read(&cert_path)
                .map_err(|e| anyhow::anyhow!("failed to read cert: {e}"))?;
            let key =
                std::fs::read(&key_path).map_err(|e| anyhow::anyhow!("failed to read key: {e}"))?;
            (cert, key)
        } else {
            let (cert, key) = generate_client_cert()?;
            if let Some(parent) = cert_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("failed to create state dir: {e}"))?;
            }
            std::fs::write(&cert_path, &cert)
                .map_err(|e| anyhow::anyhow!("failed to write cert: {e}"))?;
            write_private_key(&key_path, &key)
                .map_err(|e| anyhow::anyhow!("failed to write key: {e}"))?;
            (cert, key)
        };

        let known_servers_path = paths.known_servers_path();
        let tofu_store = crate::known_servers::load_known_servers(&known_servers_path)?;
        let host_key = format!("{addr}");
        let policy = TofuPolicy::new(host_key, {
            let store = tofu_store;
            store.known
        });
        let store_arc = policy.store();

        let ep = client_endpoint(&cert, &key)?;
        let conn = connect(&ep, addr, "rift-server", Arc::new(policy))
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

        let tofu_state = TofuState::new(store_arc, known_servers_path);

        let client = Self {
            conn,
            root_handle,
            addr,
            share_name: share_name.to_string(),
            cert,
            key: Zeroizing::new(key),
            tofu_state: Some(tofu_state),
        };

        if let Some(ref tofu) = client.tofu_state {
            tofu.save_if_dirty()?;
        }

        Ok(client)
    }
}

impl<C: RiftConnection> RiftClient<C> {
    /// Construct a client from an already-established connection and root handle.
    ///
    /// This is primarily useful for testing with mock or recording connections.
    /// For production use, prefer [`RiftClient::connect_persistent`].
    pub fn from_connection(conn: C, root_handle: Uuid) -> Self {
        Self {
            conn,
            root_handle,
            addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            share_name: String::new(),
            cert: Vec::new(),
            key: Zeroizing::new(Vec::new()),
            tofu_state: None,
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

    // -----------------------------------------------------------------------
    // Async filesystem operations (generic over any RiftConnection)
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
            .map_err(|e| anyhow::Error::from(e).context("stat: open stream"))?;

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
            .map_err(|e| anyhow::Error::from(e).context("lookup: open stream"))?;

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
            .map_err(|e| anyhow::Error::from(e).context("readdir: open stream"))?;

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
            .map_err(|e| anyhow::Error::from(e).context("read_chunks: open stream"))?;

        let req = ReadRequest {
            handle: handle.as_bytes().to_vec(),
            start_chunk,
            chunk_count,
        };
        stream
            .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        tracing::info!(
            handle = %handle,
            start_chunk,
            chunk_count,
            "requesting chunks"
        );

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

        let mut chunks = Vec::with_capacity(chunk_count as usize);
        for _i in 0..chunk_count {
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

            let data_frame = stream
                .recv_frame()
                .await?
                .ok_or_else(|| anyhow::anyhow!("read_chunks: missing BLOCK_DATA"))?;
            let (_data_type, data_payload) = data_frame;

            chunks.push(ChunkData {
                index,
                length,
                hash,
                data: data_payload,
            });
        }

        let root_frame = stream
            .recv_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("read_chunks: missing TRANSFER_COMPLETE"))?;
        let (_root_type, root_payload) = root_frame;

        let transfer_complete =
            TransferComplete::decode(root_payload.as_ref()).map_err(|_| FsError::Io)?;

        tracing::info!(
            handle = %handle,
            chunk_count = chunks.len(),
            merkle_root = %hex::encode(&transfer_complete.merkle_root),
            "read_chunks complete"
        );

        Ok(ChunkReadResult {
            chunks,
            merkle_root: transfer_complete.merkle_root,
        })
    }

    /// Read chunks from a file with streaming callback.
    ///
    /// Each chunk is hash-verified immediately after receipt. The callback
    /// `on_chunk` is called for every verified chunk, allowing incremental
    /// processing (e.g., caching to disk) without buffering all chunks in memory.
    ///
    /// - `handle`: The file handle
    /// - `start_chunk`: First chunk index (0 = from beginning)
    /// - `chunk_count`: Number of chunks to read (0 = all remaining)
    /// - `on_chunk`: Callback invoked for each verified chunk
    ///
    /// Returns the file's Merkle root hash from `TRANSFER_COMPLETE`.
    #[instrument(skip(self, on_chunk))]
    pub async fn read_chunks_streaming<F>(
        &self,
        handle: Uuid,
        start_chunk: u32,
        chunk_count: u32,
        mut on_chunk: F,
    ) -> Result<Vec<u8>>
    where
        F: FnMut(ChunkData) -> anyhow::Result<()>,
    {
        let mut stream =
            self.conn.open_stream().await.map_err(|e| {
                anyhow::Error::from(e).context("read_chunks_streaming: open stream")
            })?;

        let req = ReadRequest {
            handle: handle.as_bytes().to_vec(),
            start_chunk,
            chunk_count,
        };
        stream
            .send_frame(msg::READ_REQUEST, &req.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        tracing::info!(
            handle = %handle,
            start_chunk,
            chunk_count,
            "requesting chunks (streaming)"
        );

        let (_, payload) = stream.recv_frame().await?.ok_or_else(|| {
            anyhow::anyhow!("read_chunks_streaming: server closed stream without response")
        })?;

        let response =
            ReadResponse::decode(payload.as_ref()).map_err(|_| anyhow::Error::from(FsError::Io))?;

        let chunk_count = match response.result {
            Some(read_response::Result::Ok(success)) => success.chunk_count,
            Some(read_response::Result::Error(e)) => return Err(map_proto_error(e.code)),
            None => return Err(anyhow::Error::from(FsError::Io)),
        };

        for _i in 0..chunk_count {
            let header_frame = stream
                .recv_frame()
                .await?
                .ok_or_else(|| anyhow::anyhow!("read_chunks_streaming: missing BLOCK_HEADER"))?;
            let (_header_type, header_payload) = header_frame;

            let block_header = BlockHeader::decode(header_payload.as_ref())
                .map_err(|_| anyhow::Error::from(FsError::Io))?;
            let chunk_info = block_header
                .chunk
                .ok_or_else(|| anyhow::anyhow!("read_chunks_streaming: missing ChunkInfo"))?;

            let index = chunk_info.index;
            let length = chunk_info.length;
            let hash: [u8; 32] = chunk_info
                .hash
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("read_chunks_streaming: invalid hash length"))?;

            let data_frame = stream
                .recv_frame()
                .await?
                .ok_or_else(|| anyhow::anyhow!("read_chunks_streaming: missing BLOCK_DATA"))?;
            let (_data_type, data_payload) = data_frame;

            // Verify declared length matches actual data length before hashing.
            // A mismatched length is a protocol violation — the hash can't be
            // trusted to match either declared or actual length.
            if data_payload.len() as u64 != length {
                return Err(anyhow::anyhow!(
                    "read_chunks_streaming: chunk {} declared length {} but received {} bytes",
                    index,
                    length,
                    data_payload.len()
                ));
            }

            // Verify hash immediately before callback
            let computed_hash = rift_common::crypto::Blake3Hash::new(&data_payload);
            let expected_hash = rift_common::crypto::Blake3Hash::from_slice(&hash)
                .map_err(|e| anyhow::anyhow!("read_chunks_streaming: {}", e))?;
            if computed_hash != expected_hash {
                return Err(anyhow::anyhow!(
                    "read_chunks_streaming: hash mismatch for chunk {}",
                    index
                ));
            }

            let chunk = ChunkData {
                index,
                length,
                hash,
                data: data_payload,
            };

            on_chunk(chunk)?;
        }

        let root_frame = stream
            .recv_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("read_chunks_streaming: missing TRANSFER_COMPLETE"))?;
        let (_root_type, root_payload) = root_frame;

        let transfer_complete =
            TransferComplete::decode(root_payload.as_ref()).map_err(|_| FsError::Io)?;

        tracing::info!(
            handle = %handle,
            chunk_count,
            merkle_root = %hex::encode(&transfer_complete.merkle_root),
            "read_chunks_streaming complete"
        );

        Ok(transfer_complete.merkle_root)
    }

    // ---------------------------------------------------------------------------
    // MerkleDrill
    // ---------------------------------------------------------------------------

    /// Fetch children of a node in the Merkle tree from the server.
    ///
    /// - `handle`: The file handle
    /// - `hash`: Hash of the node to query (empty = request root's children)
    ///
    /// Returns `MerkleDrillResponse` with `parent_hash` and children list.
    #[instrument(skip(self), fields(hash_len = hash.len()))]
    pub async fn merkle_drill(&self, handle: Uuid, hash: &[u8]) -> Result<MerkleDrillResponse> {
        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::Error::from(e).context("merkle_drill: open stream"))?;

        let req = MerkleDrill {
            handle: handle.as_bytes().to_vec(),
            hash: hash.to_vec(),
        };
        stream
            .send_frame(msg::MERKLE_DRILL, &req.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        let (_, payload) = stream.recv_frame().await?.ok_or_else(|| {
            anyhow::anyhow!("merkle_drill: server closed stream without response")
        })?;

        let response = MerkleDrillResponse::decode(payload.as_ref()).map_err(|_| FsError::Io)?;

        Ok(response)
    }

    /// Query the server for its identity (fingerprint, common name, available shares).
    pub async fn whoami(&self) -> Result<WhoamiResponse> {
        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::Error::from(e).context("whoami: open stream"))?;

        let req = WhoamiRequest {};
        stream
            .send_frame(msg::WHOAMI_REQUEST, &req.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        let (_, payload) = stream
            .recv_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("whoami: server closed stream without response"))?;

        let response = WhoamiResponse::decode(payload.as_ref())
            .map_err(|_| anyhow::Error::from(FsError::Io))?;

        Ok(response)
    }

    /// Return the list of shares available on the server.
    ///
    /// Sends a `WhoamiRequest` and returns `available_shares` from the response.
    pub async fn discover(&self) -> Result<Vec<ShareInfo>> {
        let response = self.whoami().await?;
        Ok(response.available_shares)
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
            .map_err(|e| anyhow::Error::from(e).context("stat_batch: open stream"))?;

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

// TODO: QuicConnection doesn't support stats; use RecordingConnection for stat tracking.
// These implementations are stubs that always return zero/empty — they do NOT reflect
// real connection activity.
impl ConnectionStats for QuicConnection {
    fn stream_count(&self) -> usize {
        0
    }
    fn recorded_frames(&self) -> Vec<rift_transport::FrameRecord> {
        Vec::new()
    }
}

/// Test helpers for RiftClient backed by `RecordingConnection`.
/// These are separate from the main impl block because `RecordingConnection`
/// is specifically designed for testing.
impl<C: RiftConnection> RiftClient<rift_transport::RecordingConnection<C>> {
    /// Get the number of times `open_stream` was called on the underlying connection.
    pub fn stream_count(&self) -> usize {
        self.conn.stream_count()
    }

    /// Access the frames recorded by the underlying connection.
    pub fn recorded_frames(&self) -> Vec<rift_transport::FrameRecord> {
        self.conn.recorded_frames()
    }
}

// ---------------------------------------------------------------------------
// RemoteShare impl (Linux only)
// ---------------------------------------------------------------------------

/// Wrapper type for `MerkleDrill` results, simplifying the protocol response.
pub struct MerkleDrillResult {
    pub parent_hash: Vec<u8>,
    pub children: Vec<MerkleChildInfo>,
}

/// Information about a child node returned from merkle drill.
pub struct MerkleChildInfo {
    pub is_subtree: bool,
    pub hash: Vec<u8>,
    pub length: u64,
    pub chunk_index: u32,
}

impl From<MerkleDrillResponse> for MerkleDrillResult {
    fn from(resp: MerkleDrillResponse) -> Self {
        let children: Vec<MerkleChildInfo> = resp
            .children
            .iter()
            .map(|c| MerkleChildInfo {
                is_subtree: c.child_type == MerkleChildType::MerkleChildSubtree as i32,
                hash: c.hash.clone(),
                length: c.length,
                chunk_index: c.chunk_index,
            })
            .collect();
        Self {
            parent_hash: resp.parent_hash,
            children,
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

    async fn read_chunk(&self, handle: Uuid, chunk_index: u32) -> anyhow::Result<ChunkReadResult> {
        self.read_chunks(handle, chunk_index, 1).await
    }

    async fn read_chunks_streaming(
        &self,
        handle: Uuid,
        start_chunk: u32,
        chunk_count: u32,
        on_chunk: Box<dyn FnMut(ChunkData) -> anyhow::Result<()> + Send>,
    ) -> anyhow::Result<Vec<u8>> {
        self.read_chunks_streaming(handle, start_chunk, chunk_count, on_chunk)
            .await
    }

    async fn merkle_drill(&self, handle: Uuid, hash: &[u8]) -> anyhow::Result<MerkleDrillResult> {
        let resp = self.merkle_drill(handle, hash).await?;
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

/// Generate an ephemeral self-signed TLS certificate.
///
/// Used by [`RiftClient::connect`] for quick testing. Production clients
/// should use [`RiftClient::connect_persistent`] instead, which loads or
/// generates a persistent certificate with a stable fingerprint.
fn generate_client_cert() -> Result<(Vec<u8>, Vec<u8>)> {
    let cert = rcgen::generate_simple_self_signed(vec!["rift-client".to_string()])?;
    Ok((cert.cert.der().to_vec(), cert.key_pair.serialize_der()))
}

/// Write a private key file with owner-only read/write permissions (0o600 on Unix).
#[cfg(unix)]
fn write_private_key(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(data)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_key(path: &std::path::Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as ProstMessage;
    use rift_common::FsError;
    use rift_protocol::messages::{
        lookup_response, readdir_response, stat_result, ErrorCode, ErrorDetail, FileAttrs,
        FileType, LookupResponse, LookupResult, ReaddirEntry, ReaddirResponse, ReaddirSuccess,
        ShareInfo, StatRequest, StatResponse, StatResult, WhoamiResponse,
    };
    use rift_transport::connection::InMemoryConnection;
    use rift_transport::RecordingConnection;
    use uuid::Uuid;

    fn dummy_root() -> Uuid {
        Uuid::from_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
    }

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

    // -----------------------------------------------------------------------
    // Group A: Construction and accessors
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn from_connection_stores_root_handle() {
        let (client_conn, _server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);
        assert_eq!(client.root_handle(), root);
    }

    #[tokio::test]
    async fn server_fingerprint_returns_peer_fingerprint() {
        let (client_conn, _server_conn) = InMemoryConnection::pair();
        let client = RiftClient::from_connection(client_conn, dummy_root());
        assert_eq!(client.server_fingerprint(), "test-server-fingerprint");
    }

    #[tokio::test]
    async fn close_connection_causes_operations_to_fail() {
        let (client_conn, _server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);
        client.close_connection();
        let result = client.stat(root).await;
        assert!(result.is_err(), "stat after close_connection must fail");
    }

    // -----------------------------------------------------------------------
    // Group B: stat(), lookup(), readdir()
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn stat_valid_handle_returns_file_attrs() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();
            let response = StatResponse {
                results: vec![StatResult {
                    handle: vec![],
                    result: Some(stat_result::Result::Attrs(FileAttrs {
                        size: 42,
                        file_type: FileType::Regular as i32,
                        ..Default::default()
                    })),
                }],
            };
            stream
                .send_frame(msg::STAT_RESPONSE, &response.encode_to_vec())
                .await
                .unwrap();
        });

        let attrs = client.stat(root).await.unwrap();
        assert_eq!(attrs.size, 42);
        assert_eq!(attrs.file_type, FileType::Regular as i32);
    }

    #[tokio::test]
    async fn stat_error_response_returns_err() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();
            let response = StatResponse {
                results: vec![StatResult {
                    handle: vec![],
                    result: Some(stat_result::Result::Error(ErrorDetail {
                        code: ErrorCode::ErrorNotFound as i32,
                        message: String::new(),
                        metadata: None,
                    })),
                }],
            };
            stream
                .send_frame(msg::STAT_RESPONSE, &response.encode_to_vec())
                .await
                .unwrap();
        });

        let result = client.stat(root).await;
        assert!(result.is_err(), "error response must yield Err");
        let fs_err = result.unwrap_err().downcast::<FsError>().unwrap();
        assert!(matches!(fs_err, FsError::NotFound));
    }

    #[tokio::test]
    async fn lookup_success_returns_handle_and_attrs() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);
        let child_uuid = Uuid::from_bytes([2u8; 16]);

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();
            let response = LookupResponse {
                result: Some(lookup_response::Result::Entry(LookupResult {
                    handle: child_uuid.as_bytes().to_vec(),
                    attrs: Some(FileAttrs {
                        size: 100,
                        file_type: FileType::Regular as i32,
                        ..Default::default()
                    }),
                })),
            };
            stream
                .send_frame(msg::LOOKUP_RESPONSE, &response.encode_to_vec())
                .await
                .unwrap();
        });

        let (handle, attrs) = client.lookup(root, "file.txt").await.unwrap();
        assert_eq!(handle, child_uuid);
        assert_eq!(attrs.size, 100);
        assert_eq!(attrs.file_type, FileType::Regular as i32);
    }

    #[tokio::test]
    async fn lookup_not_found_returns_err() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();
            let response = LookupResponse {
                result: Some(lookup_response::Result::Error(ErrorDetail {
                    code: ErrorCode::ErrorNotFound as i32,
                    message: String::new(),
                    metadata: None,
                })),
            };
            stream
                .send_frame(msg::LOOKUP_RESPONSE, &response.encode_to_vec())
                .await
                .unwrap();
        });

        let result = client.lookup(root, "missing.txt").await;
        assert!(result.is_err());
        let fs_err = result.unwrap_err().downcast::<FsError>().unwrap();
        assert!(matches!(fs_err, FsError::NotFound));
    }

    #[tokio::test]
    async fn readdir_returns_entry_list() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();
            let response = ReaddirResponse {
                result: Some(readdir_response::Result::Entries(ReaddirSuccess {
                    entries: vec![
                        ReaddirEntry {
                            name: "alpha.txt".to_string(),
                            file_type: FileType::Regular as i32,
                            handle: vec![1u8; 16],
                        },
                        ReaddirEntry {
                            name: "beta.txt".to_string(),
                            file_type: FileType::Regular as i32,
                            handle: vec![2u8; 16],
                        },
                    ],
                    has_more: false,
                })),
            };
            stream
                .send_frame(msg::READDIR_RESPONSE, &response.encode_to_vec())
                .await
                .unwrap();
        });

        let entries = client.readdir(root).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "alpha.txt");
        assert_eq!(entries[1].name, "beta.txt");
    }

    // -----------------------------------------------------------------------
    // Group C: stat_batch()
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn stat_batch_sends_single_request_with_all_handles() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let recording = RecordingConnection::new(client_conn);
        let client = RiftClient::from_connection(recording, dummy_root());

        let handle1 = Uuid::from_bytes([1u8; 16]);
        let handle2 = Uuid::from_bytes([2u8; 16]);

        let server_task = tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let (_, payload) = stream.recv_frame().await.unwrap().unwrap();
            let req = StatRequest::decode(payload.as_ref()).unwrap();
            assert_eq!(req.handles.len(), 2);
            assert_eq!(req.handles[0], handle1.as_bytes().as_slice());
            assert_eq!(req.handles[1], handle2.as_bytes().as_slice());
            let response = StatResponse {
                results: vec![
                    StatResult {
                        handle: vec![1u8; 16],
                        result: Some(stat_result::Result::Attrs(FileAttrs {
                            size: 10,
                            ..Default::default()
                        })),
                    },
                    StatResult {
                        handle: vec![2u8; 16],
                        result: Some(stat_result::Result::Attrs(FileAttrs {
                            size: 20,
                            ..Default::default()
                        })),
                    },
                ],
            };
            stream
                .send_frame(msg::STAT_RESPONSE, &response.encode_to_vec())
                .await
                .unwrap();
        });

        let results = client.stat_batch(vec![handle1, handle2]).await.unwrap();
        assert_eq!(results.len(), 2);

        let frames = client.recorded_frames();
        assert_eq!(frames.len(), 1, "stat_batch must send exactly 1 frame");
        assert_eq!(frames[0].type_id, msg::STAT_REQUEST);

        let req = StatRequest::decode(frames[0].payload.as_ref()).unwrap();
        assert_eq!(req.handles.len(), 2, "request must include both handles");

        server_task.await.expect("server task panicked");
    }

    #[tokio::test]
    async fn stat_batch_empty_input_returns_empty_vec() {
        let (client_conn, _server_conn) = InMemoryConnection::pair();
        let client = RiftClient::from_connection(client_conn, dummy_root());
        let results = client.stat_batch(vec![]).await.unwrap();
        assert!(results.is_empty());
    }

    // -----------------------------------------------------------------------
    // Group D: discover() and whoami()
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn discover_returns_share_list() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let client = RiftClient::from_connection(client_conn, dummy_root());

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();
            let response = WhoamiResponse {
                fingerprint: "server-fp".to_string(),
                common_name: "rift-server".to_string(),
                available_shares: vec![
                    ShareInfo {
                        name: "share-a".to_string(),
                        read_only: true,
                        ..Default::default()
                    },
                    ShareInfo {
                        name: "share-b".to_string(),
                        read_only: false,
                        ..Default::default()
                    },
                ],
            };
            stream
                .send_frame(msg::WHOAMI_RESPONSE, &response.encode_to_vec())
                .await
                .unwrap();
        });

        let shares = client.discover().await.unwrap();
        assert_eq!(shares.len(), 2);
        assert_eq!(shares[0].name, "share-a");
        assert_eq!(shares[1].name, "share-b");
    }

    #[tokio::test]
    async fn whoami_returns_identity() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let client = RiftClient::from_connection(client_conn, dummy_root());

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();
            let response = WhoamiResponse {
                fingerprint: "fp123".to_string(),
                common_name: "Alice".to_string(),
                available_shares: vec![],
            };
            stream
                .send_frame(msg::WHOAMI_RESPONSE, &response.encode_to_vec())
                .await
                .unwrap();
        });

        let identity = client.whoami().await.unwrap();
        assert_eq!(identity.fingerprint, "fp123");
        assert_eq!(identity.common_name, "Alice");
        assert!(identity.available_shares.is_empty());
    }

    // -----------------------------------------------------------------------
    // Group E: ConnectionStats via RecordingConnection<InMemoryConnection>
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn recording_client_stream_count_starts_at_zero() {
        let (inner, _server) = InMemoryConnection::pair();
        let recording = RecordingConnection::new(inner);
        let client = RiftClient::from_connection(recording, dummy_root());
        assert_eq!(client.stream_count(), 0);
    }

    #[tokio::test]
    async fn recording_client_stream_count_increments_after_operation() {
        let (inner, server_conn) = InMemoryConnection::pair();
        let recording = RecordingConnection::new(inner);
        let root = dummy_root();
        let client = RiftClient::from_connection(recording, root);

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();
            let response = StatResponse {
                results: vec![StatResult {
                    handle: vec![],
                    result: Some(stat_result::Result::Attrs(FileAttrs {
                        size: 1,
                        ..Default::default()
                    })),
                }],
            };
            stream
                .send_frame(msg::STAT_RESPONSE, &response.encode_to_vec())
                .await
                .unwrap();
        });

        client.stat(root).await.unwrap();
        assert_eq!(
            client.stream_count(),
            1,
            "stream_count must be exactly 1 after one stat"
        );
    }

    #[tokio::test]
    async fn recording_client_recorded_frames_captures_sent_frames() {
        let (inner, server_conn) = InMemoryConnection::pair();
        let recording = RecordingConnection::new(inner);
        let root = dummy_root();
        let client = RiftClient::from_connection(recording, root);

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();
            let response = StatResponse {
                results: vec![StatResult {
                    handle: vec![],
                    result: Some(stat_result::Result::Attrs(FileAttrs {
                        size: 7,
                        ..Default::default()
                    })),
                }],
            };
            stream
                .send_frame(msg::STAT_RESPONSE, &response.encode_to_vec())
                .await
                .unwrap();
        });

        client.stat(root).await.unwrap();

        let frames = client.recorded_frames();
        assert!(
            !frames.is_empty(),
            "recorded_frames must not be empty after stat"
        );
        assert_eq!(
            frames[0].type_id,
            msg::STAT_REQUEST,
            "first recorded frame must be STAT_REQUEST"
        );
    }

    // -----------------------------------------------------------------------
    // Group F: ChunkData Bytes type (rift-b0er.2.2)
    // -----------------------------------------------------------------------

    use tracing_test::traced_test;

    #[test]
    fn chunk_data_data_field_is_bytes() {
        use bytes::Bytes;

        // This test verifies that ChunkData.data is of type Bytes.
        // It should compile and pass after changing Vec<u8> to Bytes.
        let chunk = ChunkData {
            index: 0,
            length: 3,
            hash: [1u8; 32],
            data: Bytes::from(vec![1u8, 2, 3]),
        };

        // Verify the data is accessible as Bytes
        assert_eq!(chunk.data.len(), 3);
        assert_eq!(&chunk.data[..], &[1u8, 2, 3]);

        // Verify Bytes can be cloned cheaply (reference-counted)
        let data_clone = chunk.data.clone();
        assert_eq!(data_clone, chunk.data);
    }

    // -----------------------------------------------------------------------
    // Group G: read_chunks INFO log events
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[traced_test]
    async fn read_chunks_emits_info_logs() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);

        let chunk_data = b"hello";
        let chunk_hash: [u8; 32] = rift_common::crypto::Blake3Hash::new(chunk_data)
            .as_bytes()
            .to_owned();
        let merkle_root = chunk_hash.to_vec();

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();

            // Send READ_RESPONSE with chunk_count=1
            let read_response = ReadResponse {
                result: Some(read_response::Result::Ok(
                    rift_protocol::messages::ReadSuccess { chunk_count: 1 },
                )),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &read_response.encode_to_vec())
                .await
                .unwrap();

            // Send BLOCK_HEADER with ChunkInfo
            let block_header = BlockHeader {
                chunk: Some(rift_protocol::messages::ChunkInfo {
                    index: 0,
                    length: chunk_data.len() as u64,
                    hash: chunk_hash.to_vec(),
                }),
            };
            stream
                .send_frame(msg::BLOCK_HEADER, &block_header.encode_to_vec())
                .await
                .unwrap();

            // Send BLOCK_DATA with chunk bytes
            stream
                .send_frame(msg::BLOCK_DATA, chunk_data)
                .await
                .unwrap();

            // Send TRANSFER_COMPLETE with merkle_root
            let transfer_complete = TransferComplete {
                merkle_root: merkle_root.clone(),
            };
            stream
                .send_frame(msg::TRANSFER_COMPLETE, &transfer_complete.encode_to_vec())
                .await
                .unwrap();
        });

        let uuid = Uuid::from_bytes([0x42u8; 16]);
        let result = client.read_chunks(uuid, 0, 1).await;
        assert!(result.is_ok(), "read_chunks should succeed");

        // Check for INFO log with "requesting chunks"
        logs_assert(|lines: &[&str]| {
            let has_requesting = lines
                .iter()
                .any(|l| l.contains(" INFO ") && l.contains("requesting chunks"));
            if has_requesting {
                Ok(())
            } else {
                Err("missing INFO log with 'requesting chunks'".to_string())
            }
        });

        // Check for INFO log with "read_chunks complete"
        logs_assert(|lines: &[&str]| {
            let has_complete = lines
                .iter()
                .any(|l| l.contains(" INFO ") && l.contains("read_chunks complete"));
            if has_complete {
                Ok(())
            } else {
                Err("missing INFO log with 'read_chunks complete'".to_string())
            }
        });
    }

    // -----------------------------------------------------------------------
    // Group H: read_chunks_streaming (rift-b0er.2.6)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_chunks_streaming_callback_invoked_exactly_chunk_count_times() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);

        let chunk1_data = b"chunk one";
        let chunk2_data = b"chunk two";
        let chunk3_data = b"chunk three";
        let chunk1_hash: [u8; 32] = rift_common::crypto::Blake3Hash::new(chunk1_data)
            .as_bytes()
            .to_owned();
        let chunk2_hash: [u8; 32] = rift_common::crypto::Blake3Hash::new(chunk2_data)
            .as_bytes()
            .to_owned();
        let chunk3_hash: [u8; 32] = rift_common::crypto::Blake3Hash::new(chunk3_data)
            .as_bytes()
            .to_owned();
        let merkle_root = chunk1_hash.to_vec();

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();

            // Send READ_RESPONSE with chunk_count=3
            let read_response = ReadResponse {
                result: Some(read_response::Result::Ok(
                    rift_protocol::messages::ReadSuccess { chunk_count: 3 },
                )),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &read_response.encode_to_vec())
                .await
                .unwrap();

            // Send 3 chunks
            for (i, (data, hash)) in [
                (chunk1_data.as_slice(), &chunk1_hash),
                (chunk2_data.as_slice(), &chunk2_hash),
                (chunk3_data.as_slice(), &chunk3_hash),
            ]
            .iter()
            .enumerate()
            {
                let block_header = BlockHeader {
                    chunk: Some(rift_protocol::messages::ChunkInfo {
                        index: u32::try_from(i).expect("chunk index fits in u32"),
                        length: data.len() as u64,
                        hash: hash.to_vec(),
                    }),
                };
                stream
                    .send_frame(msg::BLOCK_HEADER, &block_header.encode_to_vec())
                    .await
                    .unwrap();
                stream.send_frame(msg::BLOCK_DATA, data).await.unwrap();
            }

            // Send TRANSFER_COMPLETE
            let transfer_complete = TransferComplete {
                merkle_root: merkle_root.clone(),
            };
            stream
                .send_frame(msg::TRANSFER_COMPLETE, &transfer_complete.encode_to_vec())
                .await
                .unwrap();
        });

        let uuid = Uuid::from_bytes([0x42u8; 16]);
        let mut call_count = 0u32;
        let result = client
            .read_chunks_streaming(uuid, 0, 3, |_chunk| {
                call_count += 1;
                Ok(())
            })
            .await;

        assert!(result.is_ok(), "read_chunks_streaming should succeed");
        assert_eq!(call_count, 3, "callback should be invoked exactly 3 times");
    }

    #[tokio::test]
    async fn read_chunks_streaming_verifies_hash_before_callback() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);

        let chunk_data = b"valid data here";
        let correct_hash: [u8; 32] = rift_common::crypto::Blake3Hash::new(chunk_data)
            .as_bytes()
            .to_owned();
        let wrong_hash: [u8; 32] = [0xFF; 32];
        let merkle_root = correct_hash.to_vec();

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();

            let read_response = ReadResponse {
                result: Some(read_response::Result::Ok(
                    rift_protocol::messages::ReadSuccess { chunk_count: 1 },
                )),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &read_response.encode_to_vec())
                .await
                .unwrap();

            // Send chunk with WRONG hash
            let block_header = BlockHeader {
                chunk: Some(rift_protocol::messages::ChunkInfo {
                    index: 0,
                    length: chunk_data.len() as u64,
                    hash: wrong_hash.to_vec(),
                }),
            };
            stream
                .send_frame(msg::BLOCK_HEADER, &block_header.encode_to_vec())
                .await
                .unwrap();
            stream
                .send_frame(msg::BLOCK_DATA, chunk_data)
                .await
                .unwrap();

            let transfer_complete = TransferComplete {
                merkle_root: merkle_root.clone(),
            };
            stream
                .send_frame(msg::TRANSFER_COMPLETE, &transfer_complete.encode_to_vec())
                .await
                .unwrap();
        });

        let uuid = Uuid::from_bytes([0x42u8; 16]);
        let mut callback_was_called = false;
        let result = client
            .read_chunks_streaming(uuid, 0, 1, |_chunk| {
                callback_was_called = true;
                Ok(())
            })
            .await;

        assert!(
            result.is_err(),
            "read_chunks_streaming should fail on hash mismatch"
        );
        assert!(
            !callback_was_called,
            "callback must NOT be called when hash verification fails"
        );
    }

    #[tokio::test]
    async fn read_chunks_streaming_returns_merkle_root() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);

        let chunk_data = b"single chunk";
        let chunk_hash: [u8; 32] = rift_common::crypto::Blake3Hash::new(chunk_data)
            .as_bytes()
            .to_owned();
        let merkle_root = chunk_hash.to_vec();
        let expected_root = merkle_root.clone();

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();

            let read_response = ReadResponse {
                result: Some(read_response::Result::Ok(
                    rift_protocol::messages::ReadSuccess { chunk_count: 1 },
                )),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &read_response.encode_to_vec())
                .await
                .unwrap();

            let block_header = BlockHeader {
                chunk: Some(rift_protocol::messages::ChunkInfo {
                    index: 0,
                    length: chunk_data.len() as u64,
                    hash: chunk_hash.to_vec(),
                }),
            };
            stream
                .send_frame(msg::BLOCK_HEADER, &block_header.encode_to_vec())
                .await
                .unwrap();
            stream
                .send_frame(msg::BLOCK_DATA, chunk_data)
                .await
                .unwrap();

            let transfer_complete = TransferComplete {
                merkle_root: merkle_root.clone(),
            };
            stream
                .send_frame(msg::TRANSFER_COMPLETE, &transfer_complete.encode_to_vec())
                .await
                .unwrap();
        });

        let uuid = Uuid::from_bytes([0x42u8; 16]);
        let result = client
            .read_chunks_streaming(uuid, 0, 1, |_chunk| Ok(()))
            .await;

        assert!(result.is_ok(), "read_chunks_streaming should succeed");
        assert_eq!(
            result.unwrap(),
            expected_root,
            "should return the merkle_root from TRANSFER_COMPLETE"
        );
    }

    #[tokio::test]
    async fn old_read_chunks_still_works_after_streaming_added() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);

        let chunk_data = b"hello";
        let chunk_hash: [u8; 32] = rift_common::crypto::Blake3Hash::new(chunk_data)
            .as_bytes()
            .to_owned();
        let merkle_root = chunk_hash.to_vec();
        let expected_root = merkle_root.clone();

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();

            let read_response = ReadResponse {
                result: Some(read_response::Result::Ok(
                    rift_protocol::messages::ReadSuccess { chunk_count: 1 },
                )),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &read_response.encode_to_vec())
                .await
                .unwrap();

            let block_header = BlockHeader {
                chunk: Some(rift_protocol::messages::ChunkInfo {
                    index: 0,
                    length: chunk_data.len() as u64,
                    hash: chunk_hash.to_vec(),
                }),
            };
            stream
                .send_frame(msg::BLOCK_HEADER, &block_header.encode_to_vec())
                .await
                .unwrap();
            stream
                .send_frame(msg::BLOCK_DATA, chunk_data)
                .await
                .unwrap();

            let transfer_complete = TransferComplete {
                merkle_root: merkle_root.clone(),
            };
            stream
                .send_frame(msg::TRANSFER_COMPLETE, &transfer_complete.encode_to_vec())
                .await
                .unwrap();
        });

        let uuid = Uuid::from_bytes([0x42u8; 16]);
        let result = client.read_chunks(uuid, 0, 1).await;

        assert!(result.is_ok(), "old read_chunks should still work");
        let chunk_result = result.unwrap();
        assert_eq!(chunk_result.chunks.len(), 1);
        assert_eq!(&chunk_result.chunks[0].data[..], chunk_data);
        assert_eq!(chunk_result.merkle_root, expected_root);
    }

    #[tokio::test]
    async fn read_chunks_streaming_rejects_mismatched_length() {
        let (client_conn, server_conn) = InMemoryConnection::pair();
        let root = dummy_root();
        let client = RiftClient::from_connection(client_conn, root);

        let chunk_data = b"only ten bytes"; // 14 bytes actual
        let chunk_hash: [u8; 32] = rift_common::crypto::Blake3Hash::new(chunk_data)
            .as_bytes()
            .to_owned();
        let merkle_root = chunk_hash.to_vec();

        tokio::spawn(async move {
            let mut stream = server_conn.accept_stream().await.unwrap();
            let _ = stream.recv_frame().await.unwrap().unwrap();

            let read_response = ReadResponse {
                result: Some(read_response::Result::Ok(
                    rift_protocol::messages::ReadSuccess { chunk_count: 1 },
                )),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &read_response.encode_to_vec())
                .await
                .unwrap();

            // Server LIES about length: declares 100 but sends 14 bytes
            let block_header = BlockHeader {
                chunk: Some(rift_protocol::messages::ChunkInfo {
                    index: 0,
                    length: 100, // <-- WRONG: actual data is 14 bytes
                    hash: chunk_hash.to_vec(),
                }),
            };
            stream
                .send_frame(msg::BLOCK_HEADER, &block_header.encode_to_vec())
                .await
                .unwrap();
            stream
                .send_frame(msg::BLOCK_DATA, chunk_data)
                .await
                .unwrap();

            let transfer_complete = TransferComplete {
                merkle_root: merkle_root.clone(),
            };
            stream
                .send_frame(msg::TRANSFER_COMPLETE, &transfer_complete.encode_to_vec())
                .await
                .unwrap();
        });

        let uuid = Uuid::from_bytes([0x42u8; 16]);
        let result = client
            .read_chunks_streaming(uuid, 0, 1, |_chunk| Ok(()))
            .await;

        assert!(result.is_err(), "mismatched chunk length must be rejected");
    }
}
