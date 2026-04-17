//! Pure request-handler functions for the Rift server.
//!
//! Each `*_response` function decodes a proto request from raw bytes,
//! performs the filesystem work using async I/O, and returns a proto
//! response.  The async dispatch layer in `server.rs` handles the
//! transport and calls these functions.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use futures::future::BoxFuture;
use futures::FutureExt;
use prost::Message as _;
use tracing::instrument;

use rift_common::crypto::Blake3Hash;
use rift_protocol::messages::{
    lookup_response, msg, read_response, readdir_response, stat_result, BlockHeader, ChunkInfo,
    ErrorCode, ErrorDetail, FileAttrs, FileType, LookupRequest, LookupResponse, LookupResult,
    MerkleDrill, MerkleLevelResponse, ReadRequest, ReadResponse, ReadSuccess, ReaddirEntry,
    ReaddirRequest, ReaddirResponse, ReaddirSuccess, StatRequest, StatResponse, StatResult,
    TransferComplete,
};
use rift_transport::RiftStream;

use uuid::Uuid;

use crate::handle::HandleDatabase;
use crate::metadata::db::Database;

// ---------------------------------------------------------------------------
// Path resolution (security-critical)
// ---------------------------------------------------------------------------

/// Resolve an opaque `handle` (UUID from the client) to a
/// canonical filesystem path within `share` using the HandleDatabase.
///
/// # Security invariants
///
/// - Looks up path from HandleDatabase using UUID.
/// - Canonicalises the result with `tokio::fs::canonicalize`, which resolves
///   all `..` components and follows symlinks.
/// - Checks that the canonical result is prefixed by the canonical share root,
///   which rejects:
///   - Symlinks pointing outside the share.
///   - Paths that escape via intermediate symlinks.
#[instrument(skip(share, handle_db), fields(share = %share.display(), handle = ?handle), level = "debug")]
pub async fn resolve(
    share: &Path,
    handle: &Uuid,
    handle_db: &HandleDatabase,
) -> anyhow::Result<PathBuf> {
    let relative_path = match handle_db.get_path(handle) {
        Some(path) => path,
        None => {
            tracing::warn!("handle not found in database");
            anyhow::bail!("invalid handle: not found");
        }
    };

    let share_canonical = tokio::fs::canonicalize(share)
        .await
        .context("share root does not exist or is inaccessible")?;

    let joined = share_canonical.join(&relative_path);

    let canonical = tokio::fs::canonicalize(&joined)
        .await
        .with_context(|| format!("path does not exist: {}", relative_path.display()))?;

    if !canonical.starts_with(&share_canonical) {
        tracing::warn!(path = %canonical.display(), "path escapes share root");
        anyhow::bail!("path escapes share root: {}", relative_path.display());
    }

    Ok(canonical)
}

// ---------------------------------------------------------------------------
// Attribute conversion
// ---------------------------------------------------------------------------

fn root_hash_for_type(is_dir: bool) -> Blake3Hash {
    if is_dir {
        Blake3Hash::new(b"<directory>")
    } else {
        Blake3Hash::new(b"<symlink>")
    }
}

/// Convert `std::fs::Metadata` to a proto `FileAttrs` message.
///
/// Uses Unix-specific metadata fields (`mode`, `uid`, `gid`, `nlink`).
/// Uses constant hashes for directories and symlinks since they don't have content.
pub fn metadata_to_attrs(meta: &std::fs::Metadata) -> FileAttrs {
    let root_hash = root_hash_for_type(meta.is_dir());
    build_attrs(meta, root_hash)
}

/// Build `FileAttrs` from filesystem metadata and Merkle root hash.
///
/// The `root_hash` is always 32 bytes (blake3). For directories and symlinks,
/// a constant hash is used since they don't have content.
/// This is used by the delta sync protocol to identify file versions.
pub fn build_attrs(meta: &std::fs::Metadata, root_hash: Blake3Hash) -> FileAttrs {
    use std::os::unix::fs::MetadataExt as _;

    let file_type = if meta.is_dir() {
        FileType::Directory
    } else if meta.is_symlink() {
        FileType::Symlink
    } else {
        FileType::Regular
    };

    let mtime = meta.modified().ok().and_then(|t| {
        let dur = t.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some(prost_types::Timestamp {
            seconds: dur.as_secs() as i64,
            nanos: dur.subsec_nanos() as i32,
        })
    });

    FileAttrs {
        file_type: file_type as i32,
        size: meta.len(),
        mtime,
        mode: meta.mode(),
        uid: meta.uid(),
        gid: meta.gid(),
        nlinks: meta.nlink() as u32,
        root_hash: root_hash.as_bytes().to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Request handlers (pure: decode → filesystem → encode)
// ---------------------------------------------------------------------------

/// Handle a `StatRequest`: stat each requested handle and return one
/// `StatResult` per handle (success or error).
///
/// Malformed payloads return an empty result list rather than panicking.
#[instrument(skip(share, db, handle_db), fields(share = %share.display()), level = "debug")]
pub async fn stat_response(
    payload: &[u8],
    share: &Path,
    db: Option<&Database>,
    handle_db: &HandleDatabase,
) -> StatResponse {
    let req = match StatRequest::decode(payload) {
        Ok(r) => r,
        Err(_) => return StatResponse { results: vec![] },
    };

    // Build results. Each handle in the request must produce exactly one result
    // in the response, preserving the 1:1 invariant. Invalid handles (wrong
    // byte count, etc.) produce an ErrorNotFound result rather than being
    // silently dropped.
    let futures: Vec<_> = req
        .handles
        .into_iter()
        .map(|handle_bytes| match Uuid::from_slice(&handle_bytes) {
            Ok(uuid) => async_stat(share, handle_bytes, uuid, handle_db, db).boxed(),
            Err(_) => async { stat_error(ErrorCode::ErrorNotFound) }.boxed(),
        })
        .collect();

    let results = futures::future::join_all(futures).await;

    StatResponse { results }
}

/// Handle a `LookupRequest`: resolve `(parent_handle, name)` to a child
/// handle and its attributes.
///
/// Returns `ErrorNotFound` if either the parent or the child does not exist.
#[instrument(skip(share, db, handle_db), fields(share = %share.display()), level = "debug")]
pub async fn lookup_response(
    payload: &[u8],
    share: &Path,
    db: Option<&Database>,
    handle_db: &HandleDatabase,
) -> LookupResponse {
    let req = match LookupRequest::decode(payload) {
        Ok(r) => r,
        Err(_) => return lookup_error(ErrorCode::ErrorUnsupported),
    };

    // Validate the name: must be a single component (no slashes, no NUL).
    if req.name.is_empty() || req.name.contains('/') || req.name.contains('\0') {
        return lookup_error(ErrorCode::ErrorUnsupported);
    }

    // Parse the parent handle from bytes to UUID at the network boundary
    let parent_uuid = match Uuid::from_slice(&req.parent_handle) {
        Ok(u) => u,
        Err(_) => return lookup_error(ErrorCode::ErrorNotFound),
    };

    let parent_canonical = match resolve(share, &parent_uuid, handle_db).await {
        Ok(p) => p,
        Err(_) => return lookup_error(ErrorCode::ErrorNotFound),
    };

    let share_canonical = match tokio::fs::canonicalize(share).await {
        Ok(p) => p,
        Err(_) => return lookup_error(ErrorCode::ErrorUnsupported),
    };

    let child_path = parent_canonical.join(&req.name);

    let child_canonical = match tokio::fs::canonicalize(&child_path).await {
        Ok(p) => p,
        Err(_) => return lookup_error(ErrorCode::ErrorNotFound),
    };

    let symlink_out_of_the_share = !child_canonical.starts_with(&share_canonical);
    if symlink_out_of_the_share {
        return lookup_error(ErrorCode::ErrorNotFound);
    }

    let meta = match tokio::fs::metadata(&child_canonical).await {
        Ok(m) => m,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    // Get or create a handle for the child via HandleDatabase
    let handle = match handle_db
        .get_or_create_handle(&child_canonical, &share_canonical)
        .await
    {
        Ok(uuid) => uuid.as_bytes().to_vec(),
        Err(_) => return lookup_error(ErrorCode::ErrorNotFound),
    };

    let root_hash = get_or_compute_merkle_root(&child_canonical, &meta, db).await;

    LookupResponse {
        result: Some(lookup_response::Result::Entry(LookupResult {
            handle,
            attrs: Some(build_attrs(&meta, root_hash)),
        })),
    }
}

/// Handle a `ReaddirRequest`: list the directory at `directory_handle`,
/// applying `offset` and `limit` (0 = unlimited).
///
/// Entries are returned in alphabetical order for determinism.
/// Malformed payloads return an error response rather than panicking.
#[instrument(skip(share, handle_db), fields(share = %share.display()), level = "debug")]
pub async fn readdir_response(
    payload: &[u8],
    share: &Path,
    handle_db: &HandleDatabase,
) -> ReaddirResponse {
    let req = match ReaddirRequest::decode(payload) {
        Ok(r) => r,
        Err(_) => return readdir_error(ErrorCode::ErrorUnsupported),
    };

    // Parse directory handle from bytes to UUID at the network boundary
    let dir_uuid = match Uuid::from_slice(&req.directory_handle) {
        Ok(u) => u,
        Err(_) => return readdir_error(ErrorCode::ErrorNotFound),
    };

    let dir_canonical = match resolve(share, &dir_uuid, handle_db).await {
        Ok(p) => p,
        Err(_) => return readdir_error(ErrorCode::ErrorNotFound),
    };

    let share_canonical = match tokio::fs::canonicalize(share).await {
        Ok(p) => p,
        Err(_) => return readdir_error(ErrorCode::ErrorUnsupported),
    };

    // Collect entries using async functional approach with tokio
    use tokio_stream::wrappers::ReadDirStream;
    use tokio_stream::StreamExt;

    let entries: Vec<ReaddirEntry> = match tokio::fs::read_dir(&dir_canonical).await {
        Ok(read_dir) => {
            // First collect all entries with their info using then, then filter out None values
            let stream = ReadDirStream::new(read_dir);
            let entries_with_none: Vec<Option<ReaddirEntry>> = stream
                .then(|entry_result| {
                    let share_canonical = share_canonical.clone();
                    async move {
                        let entry = entry_result.ok()?;
                        let file_type = entry.file_type().await.ok()?;
                        let proto_type = if file_type.is_dir() {
                            FileType::Directory as i32
                        } else if file_type.is_symlink() {
                            FileType::Symlink as i32
                        } else {
                            FileType::Regular as i32
                        };
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let entry_path = entry.path();

                        // Get or create handle via HandleDatabase
                        let handle = handle_db
                            .get_or_create_handle(&entry_path, &share_canonical)
                            .await
                            .ok()?
                            .as_bytes()
                            .to_vec();

                        Some(ReaddirEntry {
                            name,
                            file_type: proto_type,
                            handle,
                        })
                    }
                })
                .collect()
                .await;
            // Filter out None values functionally
            entries_with_none.into_iter().flatten().collect()
        }
        Err(e) => return readdir_error(io_err_kind_to_code(e.kind())),
    };

    // Sort entries by name using functional approach
    let mut entries = entries;
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    // Apply offset.
    let offset = req.offset as usize;
    let entries: Vec<ReaddirEntry> = entries.into_iter().skip(offset).collect();

    // Apply limit (0 = return all).
    let (entries, has_more) = if req.limit > 0 && entries.len() > req.limit as usize {
        let limited: Vec<_> = entries.into_iter().take(req.limit as usize).collect();
        (limited, true)
    } else {
        (entries, false)
    };

    ReaddirResponse {
        result: Some(readdir_response::Result::Entries(ReaddirSuccess {
            entries,
            has_more,
        })),
    }
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn error_detail(code: ErrorCode) -> ErrorDetail {
    ErrorDetail {
        code: code as i32,
        message: code.as_str_name().to_string(),
        metadata: None,
    }
}

fn stat_error(code: ErrorCode) -> StatResult {
    StatResult {
        handle: Vec::new(),
        result: Some(stat_result::Result::Error(error_detail(code))),
    }
}

fn async_stat<'a>(
    share: &'a Path,
    handle_bytes: Vec<u8>,
    uuid: Uuid,
    handle_db: &'a HandleDatabase,
    db: Option<&'a Database>,
) -> BoxFuture<'a, StatResult> {
    Box::pin(async move {
        let canonical = match resolve(share, &uuid, handle_db).await {
            Ok(p) => p,
            Err(_) => {
                return stat_error(ErrorCode::ErrorNotFound);
            }
        };

        let meta = match tokio::fs::metadata(&canonical).await {
            Ok(m) => m,
            Err(_) => {
                return stat_error(ErrorCode::ErrorNotFound);
            }
        };

        let root_hash = get_or_compute_merkle_root(&canonical, &meta, db).await;
        StatResult {
            handle: handle_bytes,
            result: Some(stat_result::Result::Attrs(build_attrs(&meta, root_hash))),
        }
    })
}

fn lookup_error(code: ErrorCode) -> LookupResponse {
    LookupResponse {
        result: Some(lookup_response::Result::Error(error_detail(code))),
    }
}

fn readdir_error(code: ErrorCode) -> ReaddirResponse {
    ReaddirResponse {
        result: Some(readdir_response::Result::Error(error_detail(code))),
    }
}

/// Get or compute the Merkle root hash for a file.
///
/// Always returns a 32-byte Blake3Hash:
/// - For regular files: Merkle root computed from content (cached if possible)
/// - For non-files (directories, etc.): uses a constant sentinel hash
async fn get_or_compute_merkle_root(
    path: &Path,
    meta: &std::fs::Metadata,
    db: Option<&Database>,
) -> Blake3Hash {
    use rift_common::crypto::{Chunker, MerkleTree};

    if !meta.is_file() {
        return root_hash_for_type(true);
    }

    if let Some(database) = db {
        match database.get_merkle(path).await {
            Ok(Some(entry)) => return entry.root,
            Ok(None) => {}
            Err(_) => {}
        }
    }

    let content = match tokio::fs::read(path).await {
        Ok(c) => c,
        Err(_) => return root_hash_for_type(true),
    };

    let chunker = Chunker::default();
    let chunk_boundaries = chunker.chunk(&content);

    let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
        .iter()
        .map(|(offset, length)| {
            let chunk_data = &content[*offset..*offset + length];
            Blake3Hash::new(chunk_data)
        })
        .collect();

    let merkle = MerkleTree::default();
    let root = merkle.build(&leaf_hashes);

    if let Some(database) = db {
        if let Ok(file_meta) = tokio::fs::metadata(path).await {
            let mtime_ns = match file_meta.modified() {
                Ok(t) => t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0),
                Err(_) => 0,
            };
            let file_size = file_meta.len();
            let _ = database
                .put_merkle(path, mtime_ns, file_size, &root, &leaf_hashes)
                .await;
        }
    }

    root
}

fn io_err_kind_to_code(kind: std::io::ErrorKind) -> ErrorCode {
    match kind {
        std::io::ErrorKind::NotFound => ErrorCode::ErrorNotFound,
        std::io::ErrorKind::PermissionDenied => ErrorCode::ErrorPermissionDenied,
        _ => ErrorCode::ErrorNotFound,
    }
}

// ---------------------------------------------------------------------------
// Read handler
// ---------------------------------------------------------------------------

#[instrument(skip_all, fields(share = %share.display()), level = "debug")]
pub async fn read_response<S: RiftStream>(
    stream: &mut S,
    payload: &[u8],
    share: &Path,
    db: Option<&Database>,
    handle_db: &HandleDatabase,
) -> anyhow::Result<()> {
    let req = match ReadRequest::decode(payload) {
        Ok(r) => r,
        Err(_) => {
            let response = ReadResponse {
                result: Some(read_response::Result::Error(ErrorDetail {
                    code: ErrorCode::ErrorUnsupported as i32,
                    message: "invalid request".to_string(),
                    metadata: None,
                })),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    // Parse handle from bytes to UUID at the network boundary
    let handle = match Uuid::from_slice(&req.handle) {
        Ok(u) => u,
        Err(_) => {
            let response = ReadResponse {
                result: Some(read_response::Result::Error(ErrorDetail {
                    code: ErrorCode::ErrorNotFound as i32,
                    message: "invalid handle".to_string(),
                    metadata: None,
                })),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    let canonical = match resolve(share, &handle, handle_db).await {
        Ok(p) => p,
        Err(_) => {
            let response = ReadResponse {
                result: Some(read_response::Result::Error(ErrorDetail {
                    code: ErrorCode::ErrorNotFound as i32,
                    message: "file not found".to_string(),
                    metadata: None,
                })),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    let content = match tokio::fs::read(&canonical).await {
        Ok(c) => c,
        Err(e) => {
            let response = ReadResponse {
                result: Some(read_response::Result::Error(ErrorDetail {
                    code: io_err_kind_to_code(e.kind()) as i32,
                    message: e.to_string(),
                    metadata: None,
                })),
            };
            stream
                .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    use rift_common::crypto::{Chunker, MerkleTree};
    let chunker = Chunker::default();
    let chunk_boundaries = chunker.chunk(&content);

    let start = req.start_chunk as usize;
    let count = if req.chunk_count == 0 {
        chunk_boundaries.len().saturating_sub(start)
    } else {
        req.chunk_count as usize
    };

    let chunks_to_read: Vec<_> = chunk_boundaries
        .iter()
        .skip(start)
        .take(count)
        .enumerate()
        .map(|(i, (offset, length))| {
            let chunk_data = &content[*offset..*offset + length];
            let hash = Blake3Hash::new(chunk_data);
            (start + i, *length, hash)
        })
        .collect();

    let chunk_count = chunks_to_read.len() as u32;

    let response = ReadResponse {
        result: Some(read_response::Result::Ok(ReadSuccess { chunk_count })),
    };
    stream
        .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
        .await?;

    for (idx, length, hash) in chunks_to_read {
        let index = idx as u32;
        let start_offset = chunk_boundaries[0..idx]
            .iter()
            .map(|(_, l)| *l)
            .sum::<usize>();
        let chunk_data = &content[start_offset..start_offset + length];

        let block_header = BlockHeader {
            chunk: Some(ChunkInfo {
                index,
                length: length as u64,
                hash: hash.as_bytes().to_vec(),
            }),
        };
        stream
            .send_frame(msg::BLOCK_HEADER, &block_header.encode_to_vec())
            .await?;

        stream.send_frame(msg::BLOCK_DATA, chunk_data).await?;
    }

    let merkle = MerkleTree::default();
    let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
        .iter()
        .map(|(offset, length)| {
            let chunk_data = &content[*offset..*offset + length];
            Blake3Hash::new(chunk_data)
        })
        .collect();
    let root = merkle.build(&leaf_hashes);

    if let Some(database) = db {
        if let Ok(file_meta) = tokio::fs::metadata(&canonical).await {
            let mtime_ns = match file_meta.modified() {
                Ok(t) => t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0),
                Err(_) => 0,
            };
            let file_size = file_meta.len();
            let _ = database
                .put_merkle(&canonical, mtime_ns, file_size, &root, &leaf_hashes)
                .await;
        }
    }

    let transfer_complete = TransferComplete {
        merkle_root: root.as_bytes().to_vec(),
    };
    stream
        .send_frame(msg::TRANSFER_COMPLETE, &transfer_complete.encode_to_vec())
        .await?;
    stream.finish_send().await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// MerkleDrill handler
// ---------------------------------------------------------------------------

#[instrument(skip_all, fields(share = %share.display()), level = "debug")]
pub async fn merkle_drill_response<S: RiftStream>(
    stream: &mut S,
    payload: &[u8],
    share: &Path,
    db: Option<&Database>,
    handle_db: &HandleDatabase,
) -> anyhow::Result<()> {
    let req = match MerkleDrill::decode(payload) {
        Ok(r) => r,
        Err(_) => {
            let response = MerkleLevelResponse {
                level: 0,
                hashes: vec![],
                subtree_bytes: vec![],
            };
            stream
                .send_frame(msg::MERKLE_LEVEL_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    // Parse handle from bytes to UUID at the network boundary
    let handle = match Uuid::from_slice(&req.handle) {
        Ok(u) => u,
        Err(_) => {
            let response = MerkleLevelResponse {
                level: req.level,
                hashes: vec![],
                subtree_bytes: vec![],
            };
            stream
                .send_frame(msg::MERKLE_LEVEL_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    let canonical = match resolve(share, &handle, handle_db).await {
        Ok(p) => p,
        Err(_) => {
            let response = MerkleLevelResponse {
                level: req.level,
                hashes: vec![],
                subtree_bytes: vec![],
            };
            stream
                .send_frame(msg::MERKLE_LEVEL_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    let content = match tokio::fs::read(&canonical).await {
        Ok(c) => c,
        Err(_) => {
            let response = MerkleLevelResponse {
                level: req.level,
                hashes: vec![],
                subtree_bytes: vec![],
            };
            stream
                .send_frame(msg::MERKLE_LEVEL_RESPONSE, &response.encode_to_vec())
                .await?;
            stream.finish_send().await?;
            return Ok(());
        }
    };

    use rift_common::crypto::{Chunker, MerkleTree};
    let chunker = Chunker::default();
    let chunk_boundaries = chunker.chunk(&content);

    let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
        .iter()
        .map(|(offset, length)| {
            let chunk_data = &content[*offset..*offset + length];
            Blake3Hash::new(chunk_data)
        })
        .collect();

    let merkle = MerkleTree::default();
    let root = merkle.build(&leaf_hashes);

    // Level 0 = root hash only
    if req.level == 0 {
        let response = MerkleLevelResponse {
            level: 0,
            hashes: vec![root.as_bytes().to_vec()],
            subtree_bytes: vec![content.len() as u64],
        };
        stream
            .send_frame(msg::MERKLE_LEVEL_RESPONSE, &response.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        if let Some(database) = db {
            if let Ok(meta) = tokio::fs::metadata(&canonical).await {
                let mtime_ns = meta
                    .modified()
                    .map(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);
                let _ = database
                    .put_merkle(&canonical, mtime_ns, meta.len(), &root, &leaf_hashes)
                    .await;
            }
        }

        return Ok(());
    }

    // Level 1 = chunk hashes (the leaf level content hashes)
    if req.level == 1 {
        let hashes: Vec<Vec<u8>> = leaf_hashes.iter().map(|h| h.as_bytes().to_vec()).collect();

        let sizes: Vec<u64> = chunk_boundaries
            .iter()
            .map(|(_, length)| *length as u64)
            .collect();

        let response = MerkleLevelResponse {
            level: 1,
            hashes,
            subtree_bytes: sizes,
        };
        stream
            .send_frame(msg::MERKLE_LEVEL_RESPONSE, &response.encode_to_vec())
            .await?;
        stream.finish_send().await?;

        return Ok(());
    }

    // For other levels, return empty for now
    let response = MerkleLevelResponse {
        level: req.level,
        hashes: vec![],
        subtree_bytes: vec![],
    };
    stream
        .send_frame(msg::MERKLE_LEVEL_RESPONSE, &response.encode_to_vec())
        .await?;
    stream.finish_send().await?;

    Ok(())
}
