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
    let stored_path = match handle_db.get_path(handle) {
        Some(path) => path,
        None => {
            tracing::warn!("handle not found in database");
            anyhow::bail!("invalid handle: not found");
        }
    };

    let share_canonical = tokio::fs::canonicalize(share)
        .await
        .context("share root does not exist or is inaccessible")?;

    let canonical = match tokio::fs::canonicalize(&stored_path).await {
        Ok(p) => p,
        Err(e) => {
            if let Some(_removed) = handle_db.remove(handle) {
                tracing::info!(handle = %handle, "evicted stale handle");
            }
            return Err(e)
                .with_context(|| format!("path does not exist: {}", stored_path.display()));
        }
    };

    if !canonical.starts_with(&share_canonical) {
        tracing::warn!(path = %canonical.display(), "path escapes share root");
        if let Some(_removed) = handle_db.remove(handle) {
            tracing::info!(handle = %handle, "evicted handle that escaped share root");
        }
        anyhow::bail!("path escapes share root: {}", stored_path.display());
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

    let handle = match handle_db.get_or_create_handle(&child_canonical).await {
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

    // Collect entries using async functional approach with tokio
    use tokio_stream::wrappers::ReadDirStream;
    use tokio_stream::StreamExt;

    let entries: Vec<ReaddirEntry> = match tokio::fs::read_dir(&dir_canonical).await {
        Ok(read_dir) => {
            // First collect all entries with their info using then, then filter out None values
            let stream = ReadDirStream::new(read_dir);
            let entries_with_none: Vec<Option<ReaddirEntry>> = stream
                .then(|entry_result| async move {
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

                    let entry_canonical = match tokio::fs::canonicalize(&entry_path).await {
                        Ok(p) => p,
                        Err(_) => return None,
                    };

                    let handle = handle_db
                        .get_or_create_handle(&entry_canonical)
                        .await
                        .ok()?
                        .as_bytes()
                        .to_vec();

                    Some(ReaddirEntry {
                        name,
                        file_type: proto_type,
                        handle,
                    })
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
            (start + i, *offset, *length, hash)
        })
        .collect();

    let chunk_count = chunks_to_read.len() as u32;

    let response = ReadResponse {
        result: Some(read_response::Result::Ok(ReadSuccess { chunk_count })),
    };
    stream
        .send_frame(msg::READ_RESPONSE, &response.encode_to_vec())
        .await?;

    for (idx, offset, length, hash) in chunks_to_read {
        let index = idx as u32;
        let chunk_data = &content[offset..offset + length];

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;
    use tempfile::TempDir;

    use rift_common::crypto::Blake3Hash;
    use rift_protocol::messages::{
        lookup_response, readdir_response, stat_result, FileType, LookupRequest, ReaddirRequest,
        StatRequest,
    };

    use crate::handle::HandleDatabase;

    // -----------------------------------------------------------------------
    // Group A: metadata_to_attrs() and build_attrs()
    // -----------------------------------------------------------------------

    #[test]
    fn metadata_to_attrs_regular_file_has_file_type() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file.txt");
        let content = b"hello rift handler";
        std::fs::write(&path, content).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let attrs = metadata_to_attrs(&meta);

        assert_eq!(attrs.file_type, FileType::Regular as i32);
        assert_eq!(attrs.size, content.len() as u64);
    }

    #[test]
    fn metadata_to_attrs_directory_has_dir_type() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("mydir");
        std::fs::create_dir(&dir).unwrap();

        let meta = std::fs::metadata(&dir).unwrap();
        let attrs = metadata_to_attrs(&meta);

        assert_eq!(attrs.file_type, FileType::Directory as i32);
    }

    #[test]
    fn build_attrs_includes_root_hash() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hashfile.txt");
        std::fs::write(&path, b"some content").unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let expected_hash = Blake3Hash::new(b"test");
        let attrs = build_attrs(&meta, expected_hash.clone());

        assert_eq!(attrs.root_hash, expected_hash.as_bytes().to_vec());
    }

    #[test]
    fn build_attrs_empty_file_has_zero_size() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.txt");
        std::fs::write(&path, b"").unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let attrs = build_attrs(&meta, Blake3Hash::new(b"dummy"));

        assert_eq!(attrs.size, 0);
    }

    // -----------------------------------------------------------------------
    // Group B: resolve()
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn resolve_valid_handle_returns_correct_path() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("target.txt");
        std::fs::write(&file, b"content").unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();

        let resolved = resolve(&share, &uuid, &handle_db).await.unwrap();

        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn resolve_unknown_uuid_bytes_returns_error() {
        // A valid-format UUID that was never registered in the database must
        // cause resolve() to return an error (exercises the "not found" path).
        let tmp = TempDir::new().unwrap();
        let handle_db = HandleDatabase::new();
        let unknown = Uuid::from_bytes([0xAA; 16]);
        assert!(resolve(tmp.path(), &unknown, &handle_db).await.is_err());
    }

    #[tokio::test]
    async fn resolve_unknown_uuid_returns_error() {
        let tmp = TempDir::new().unwrap();
        let handle_db = HandleDatabase::new();
        // A valid-format UUID that was never registered in the database
        let unknown = Uuid::from_bytes([0x42; 16]);
        let result = resolve(tmp.path(), &unknown, &handle_db).await;
        assert!(result.is_err(), "unknown UUID must produce an error");
    }

    // -----------------------------------------------------------------------
    // Group C: stat_response()
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn stat_response_returns_attrs_for_valid_handle() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("stat_me.txt");
        let content = b"stat content";
        std::fs::write(&file, content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();

        let req = StatRequest {
            handles: vec![uuid.as_bytes().to_vec()],
        };
        let payload = req.encode_to_vec();

        let resp = stat_response(&payload, &share, None, &handle_db).await;

        assert_eq!(resp.results.len(), 1);
        match &resp.results[0].result {
            Some(stat_result::Result::Attrs(attrs)) => {
                assert_eq!(attrs.size, content.len() as u64);
                assert_eq!(attrs.file_type, FileType::Regular as i32);
            }
            other => panic!("expected Attrs, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn stat_response_malformed_payload_returns_empty_results() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let handle_db = HandleDatabase::new();

        // Garbage bytes that cannot decode as StatRequest
        let garbage = vec![0xFF, 0xFE, 0x00, 0xAB, 0xCD];
        let resp = stat_response(&garbage, &share, None, &handle_db).await;

        // Malformed payload → decoder error → StatResponse { results: vec![] }
        assert_eq!(resp.results.len(), 0, "malformed payload must yield empty results");
    }

    // -----------------------------------------------------------------------
    // Group D: lookup_response()
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn lookup_response_existing_entry_returns_handle() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let child = share.join("child.txt");
        std::fs::write(&child, b"data").unwrap();

        let handle_db = HandleDatabase::new();
        // Register the parent directory so resolve() can find it
        let parent_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = LookupRequest {
            parent_handle: parent_uuid.as_bytes().to_vec(),
            name: "child.txt".to_string(),
        };
        let payload = req.encode_to_vec();

        let resp = lookup_response(&payload, &share, None, &handle_db).await;

        match resp.result {
            Some(lookup_response::Result::Entry(entry)) => {
                assert!(!entry.handle.is_empty(), "handle must be non-empty");
                let attrs = entry.attrs.expect("attrs must be present");
                assert_eq!(attrs.size, 4, "size must match \"data\" content");
            }
            other => panic!("expected Entry, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn lookup_response_missing_entry_returns_error() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        let handle_db = HandleDatabase::new();
        let parent_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = LookupRequest {
            parent_handle: parent_uuid.as_bytes().to_vec(),
            name: "nonexistent.txt".to_string(),
        };
        let payload = req.encode_to_vec();

        let resp = lookup_response(&payload, &share, None, &handle_db).await;

        match resp.result {
            Some(lookup_response::Result::Error(_)) => {}
            other => panic!("expected Error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn lookup_response_malformed_payload_returns_error() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let handle_db = HandleDatabase::new();

        let garbage = vec![0xFF, 0xAB, 0x00, 0x01, 0x02];
        let resp = lookup_response(&garbage, &share, None, &handle_db).await;

        // Must return an error variant, not panic
        match resp.result {
            Some(lookup_response::Result::Error(_)) => {}
            other => panic!("expected Error for garbage payload, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Group E: readdir_response()
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn readdir_response_lists_all_entries() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        std::fs::write(share.join("alpha.txt"), b"a").unwrap();
        std::fs::write(share.join("beta.txt"), b"b").unwrap();

        let handle_db = HandleDatabase::new();
        let dir_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = ReaddirRequest {
            directory_handle: dir_uuid.as_bytes().to_vec(),
            offset: 0,
            limit: 0,
        };
        let payload = req.encode_to_vec();

        let resp = readdir_response(&payload, &share, &handle_db).await;

        match resp.result {
            Some(readdir_response::Result::Entries(success)) => {
                let names: Vec<&str> =
                    success.entries.iter().map(|e| e.name.as_str()).collect();
                assert!(names.contains(&"alpha.txt"), "must list alpha.txt");
                assert!(names.contains(&"beta.txt"), "must list beta.txt");
                assert_eq!(names.len(), 2);
            }
            other => panic!("expected Entries, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn readdir_response_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        // No files created — directory is empty

        let handle_db = HandleDatabase::new();
        let dir_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = ReaddirRequest {
            directory_handle: dir_uuid.as_bytes().to_vec(),
            offset: 0,
            limit: 0,
        };
        let payload = req.encode_to_vec();

        let resp = readdir_response(&payload, &share, &handle_db).await;

        match resp.result {
            Some(readdir_response::Result::Entries(success)) => {
                assert!(success.entries.is_empty(), "empty dir must have no entries");
            }
            other => panic!("expected Entries, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn readdir_response_malformed_payload_returns_error() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let handle_db = HandleDatabase::new();

        let garbage = vec![0xFF, 0x00, 0xAB];
        let resp = readdir_response(&garbage, &share, &handle_db).await;

        // Must return an error variant, not panic
        match resp.result {
            Some(readdir_response::Result::Error(_)) => {}
            other => panic!("expected Error for garbage payload, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Note on Group F (mkdir / unlink / rmdir):
    // No handler functions for these operations exist in handler.rs yet.
    // The protocol constants (MKDIR_REQUEST, UNLINK_REQUEST, RMDIR_REQUEST)
    // are defined in rift-protocol but no server-side stubs are implemented.
    // Tests will be added once the stubs land.
    // -----------------------------------------------------------------------
}
