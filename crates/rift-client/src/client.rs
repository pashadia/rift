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
use prost::Message as _;
use tracing::instrument;
use uuid::Uuid;

use rift_common::FsError;
use rift_protocol::messages::{
    lookup_response, msg, read_response, readdir_response, stat_result, BlockHeader, ErrorCode,
    FileAttrs, LookupRequest, LookupResponse, MerkleDrill, MerkleLevelResponse, ReadRequest,
    ReadResponse, ReaddirEntry, ReaddirRequest, ReaddirResponse, ShareInfo, StatRequest,
    StatResponse, TransferComplete, WhoamiRequest, WhoamiResponse,
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
            let store = self.store.lock().unwrap();
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
    key: Vec<u8>,
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
                let store = tofu.store.lock().unwrap();
                store.known.clone()
            };
            let policy = TofuPolicy::new(format!("{}", self.addr), known);
            let store_arc = policy.store();
            let conn = connect(&ep, self.addr, "rift-server", Arc::new(policy))
                .await
                .map_err(|e| anyhow::anyhow!("QUIC reconnect to {}: {e}", self.addr))?;

            {
                let mut original = tofu.store.lock().unwrap();
                let updated = store_arc.lock().unwrap();
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
            key,
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
            key,
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
            key,
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
            addr: "127.0.0.1:0".parse().unwrap(),
            share_name: String::new(),
            cert: Vec::new(),
            key: Vec::new(),
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
                data: data_payload.to_vec(),
            });
        }

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

    /// Fetch Merkle tree levels from the server.
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

    /// Query the server for its identity (fingerprint, common name, available shares).
    pub async fn whoami(&self) -> Result<WhoamiResponse> {
        let mut stream = self
            .conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("whoami: open stream: {e}"))?;

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

/// Test helpers for RiftClient backed by RecordingConnection.
/// These are separate from the main impl block because RecordingConnection
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
        assert_eq!(client.stream_count(), 1, "stream_count must be exactly 1 after one stat");
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
        assert!(!frames.is_empty(), "recorded_frames must not be empty after stat");
        assert_eq!(
            frames[0].type_id,
            msg::STAT_REQUEST,
            "first recorded frame must be STAT_REQUEST"
        );
    }
}
