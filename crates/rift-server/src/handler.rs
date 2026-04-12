//! Pure request-handler functions for the Rift server.
//!
//! Each `*_response` function is intentionally stateless and synchronous:
//! it decodes a proto request from raw bytes, performs the filesystem work,
//! and returns a proto response.  The async dispatch layer in `server.rs`
//! handles I/O and calls these functions.
//!
//! Keeping the logic pure (no stream access, no async) makes unit tests fast
//! and free of runtime setup.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use prost::Message as _;
use tracing::instrument;

use rift_common::crypto::Blake3Hash;
use rift_protocol::messages::{
    lookup_response, msg, read_response, readdir_response, stat_result, BlockHeader, ChunkInfo,
    ErrorCode, ErrorDetail, FileAttrs, FileType, LookupRequest, LookupResponse, LookupResult,
    ReadRequest, ReadResponse, ReadSuccess, ReaddirEntry, ReaddirRequest, ReaddirResponse,
    ReaddirSuccess, StatRequest, StatResponse, StatResult, TransferComplete,
};
use rift_transport::RiftStream;

use crate::metadata::db::Database;

// ---------------------------------------------------------------------------
// Path resolution (security-critical)
// ---------------------------------------------------------------------------

/// Resolve an opaque `handle` (relative path bytes from the client) to a
/// canonical filesystem path within `share`.
///
/// # Security invariants
///
/// - Rejects handles containing null bytes (would truncate OS paths).
/// - Rejects empty handles.
/// - Canonicalises the result with `std::fs::canonicalize`, which resolves
///   all `..` components and follows symlinks.
/// - Checks that the canonical result is prefixed by the canonical share root,
///   which rejects:
///   - Direct `..` traversal (`../../etc/passwd`).
///   - Absolute handles (`/etc/passwd` — `Path::join` replaces the base).
///   - Intermediate symlinks pointing outside the share.
///
/// # TODO(handles)
///
/// This function exists because handles are currently relative path strings
/// (e.g. `b"subdir/file.txt"`).  Once handles become server-assigned opaque
/// tokens, resolution will be a lookup in a server-side handle table rather
/// than a filesystem path join.  The security invariants enforced here will
/// move into the handle-issuance logic instead.
#[instrument(skip(share), fields(share = %share.display(), handle = ?handle), level = "debug")]
pub fn resolve(share: &Path, handle: &[u8]) -> anyhow::Result<PathBuf> {
    if handle.contains(&0) {
        tracing::warn!(handle = ?handle, "rejecting null byte in handle");
        anyhow::bail!("null byte in handle");
    }
    if handle.is_empty() {
        tracing::warn!("rejecting empty handle");
        anyhow::bail!("empty handle");
    }

    let handle_str = std::str::from_utf8(handle).context("handle is not valid UTF-8")?;

    // Canonicalize the share root once so the prefix check is reliable even
    // when `share` itself contains symlinks or `.` components.
    let share_canonical = share
        .canonicalize()
        .context("share root does not exist or is inaccessible")?;

    // `Path::join` with an absolute component replaces the entire path — an
    // absolute handle like `/etc/passwd` would yield `/etc/passwd`, which the
    // subsequent prefix check will reject.
    let joined = share.join(handle_str);

    // `canonicalize` resolves `..` and symlinks.  It fails if the path does
    // not exist, which is the correct error for lookups of missing entries.
    let canonical = joined
        .canonicalize()
        .with_context(|| format!("path does not exist: {handle_str}"))?;

    if !canonical.starts_with(&share_canonical) {
        tracing::warn!(path = %canonical.display(), "path escapes share root");
        anyhow::bail!("path escapes share root: {handle_str}");
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
#[instrument(skip(share, db), fields(share = %share.display()), level = "debug")]
pub fn stat_response(payload: &[u8], share: &Path, db: Option<&Database>) -> StatResponse {
    let req = match StatRequest::decode(payload) {
        Ok(r) => r,
        Err(_) => return StatResponse { results: vec![] },
    };

    let results = req
        .handles
        .into_iter()
        .map(|handle| {
            let canonical = match resolve(share, &handle) {
                Ok(p) => p,
                Err(_) => {
                    return stat_error(io_error_code(&handle, share));
                }
            };

            let meta = match std::fs::metadata(&canonical) {
                Ok(m) => m,
                Err(_) => {
                    return stat_error(io_error_code(&handle, share));
                }
            };

            let root_hash = get_or_compute_merkle_root(&canonical, &meta, db);
            StatResult {
                result: Some(stat_result::Result::Attrs(build_attrs(&meta, root_hash))),
            }
        })
        .collect();

    StatResponse { results }
}

/// Handle a `LookupRequest`: resolve `(parent_handle, name)` to a child
/// handle and its attributes.
///
/// Returns `ErrorNotFound` if either the parent or the child does not exist.
#[instrument(skip(share, db), fields(share = %share.display()), level = "debug")]
pub fn lookup_response(payload: &[u8], share: &Path, db: Option<&Database>) -> LookupResponse {
    let req = match LookupRequest::decode(payload) {
        Ok(r) => r,
        Err(_) => return lookup_error(ErrorCode::ErrorUnsupported),
    };

    // Validate the name: must be a single component (no slashes, no NUL).
    if req.name.is_empty() || req.name.contains('/') || req.name.contains('\0') {
        return lookup_error(ErrorCode::ErrorUnsupported);
    }

    let parent_canonical = match resolve(share, &req.parent_handle) {
        Ok(p) => p,
        Err(_) => return lookup_error(ErrorCode::ErrorNotFound),
    };

    let share_canonical = match share.canonicalize() {
        Ok(p) => p,
        Err(_) => return lookup_error(ErrorCode::ErrorUnsupported),
    };

    let child_path = parent_canonical.join(&req.name);

    let child_canonical = match child_path.canonicalize() {
        Ok(p) => p,
        Err(_) => return lookup_error(ErrorCode::ErrorNotFound),
    };

    let symlink_out_of_the_share = !child_canonical.starts_with(&share_canonical);
    if symlink_out_of_the_share {
        return lookup_error(ErrorCode::ErrorNotFound);
    }

    let meta = match std::fs::metadata(&child_canonical) {
        Ok(m) => m,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    // The handle is the path relative to the share root (e.g. "subdir/file.txt").
    // TODO(handles): Replace with a server-assigned opaque handle.  Path-based
    // handles are invalidated by renames and expose filesystem structure to the
    // client.  See docs/02-protocol-design/handle-design.md.
    let handle = child_canonical
        .strip_prefix(&share_canonical)
        .map(|rel| rel.to_string_lossy().into_owned().into_bytes())
        .unwrap_or_else(|_| req.name.into_bytes());

    let root_hash = get_or_compute_merkle_root(&child_canonical, &meta, db);

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
#[instrument(skip(share), fields(share = %share.display()), level = "debug")]
pub fn readdir_response(payload: &[u8], share: &Path) -> ReaddirResponse {
    let req = match ReaddirRequest::decode(payload) {
        Ok(r) => r,
        Err(_) => return readdir_error(ErrorCode::ErrorUnsupported),
    };

    let dir_canonical = match resolve(share, &req.directory_handle) {
        Ok(p) => p,
        Err(_) => return readdir_error(ErrorCode::ErrorNotFound),
    };

    let share_canonical = match share.canonicalize() {
        Ok(p) => p,
        Err(_) => return readdir_error(ErrorCode::ErrorUnsupported),
    };

    let read_dir = match std::fs::read_dir(&dir_canonical) {
        Ok(rd) => rd,
        Err(e) => return readdir_error(io_err_kind_to_code(e.kind())),
    };

    // TODO: This collects the entire directory into memory before pagination.
    // For large directories this is wasteful and can hit the 16 MB codec limit.
    // Future work: stream entries directly, or maintain a server-side cursor
    // keyed by the `offset` field so only the requested window is materialised.
    let mut entries: Vec<ReaddirEntry> = read_dir
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let file_type = entry.file_type().ok()?;
            let proto_type = if file_type.is_dir() {
                FileType::Directory as i32
            } else if file_type.is_symlink() {
                FileType::Symlink as i32
            } else {
                FileType::Regular as i32
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            // TODO(handles): Replace with a server-assigned opaque handle.
            // Currently the handle is the path relative to the share root so
            // the client can use it in subsequent calls.  Path-based handles
            // are invalidated by renames and leak filesystem structure.
            // See docs/02-protocol-design/handle-design.md.
            let handle = entry
                .path()
                .strip_prefix(&share_canonical)
                .map(|rel| rel.to_string_lossy().into_owned().into_bytes())
                .unwrap_or_else(|_| name.as_bytes().to_vec());
            Some(ReaddirEntry {
                name,
                file_type: proto_type,
                handle,
            })
        })
        .collect();

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
        result: Some(stat_result::Result::Error(error_detail(code))),
    }
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

/// Map a handle that failed resolution to an appropriate `ErrorCode`.
///
/// We attempt to stat the raw path to distinguish "not found" from "permission
/// denied".  If neither is determinable, we default to `ErrorNotFound`.
fn io_error_code(handle: &[u8], share: &Path) -> ErrorCode {
    // Try joining without canonicalisation to get a rough error kind.
    if let Ok(s) = std::str::from_utf8(handle) {
        let p = share.join(s);
        if let Err(e) = std::fs::metadata(&p) {
            return io_err_kind_to_code(e.kind());
        }
    }
    ErrorCode::ErrorNotFound
}

/// Get or compute the Merkle root hash for a file.
///
/// Always returns a 32-byte Blake3Hash:
/// - For regular files: Merkle root computed from content (cached if possible)
/// - For non-files (directories, etc.): uses a constant sentinel hash
fn get_or_compute_merkle_root(
    path: &Path,
    meta: &std::fs::Metadata,
    db: Option<&Database>,
) -> Blake3Hash {
    use rift_common::crypto::{Chunker, MerkleTree};

    if !meta.is_file() {
        return root_hash_for_type(true);
    }

    if let Some(database) = db {
        match database.get_merkle(path) {
            Ok(Some(entry)) => return entry.root,
            Ok(None) => {}
            Err(_) => {}
        }
    }

    let content = match std::fs::read(path) {
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
        if let Ok(file_meta) = std::fs::metadata(path) {
            let mtime_ns = match file_meta.modified() {
                Ok(t) => t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0),
                Err(_) => 0,
            };

            let file_size = file_meta.len();
            let _ = database.put_merkle(path, mtime_ns, file_size, &root, &leaf_hashes);
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

    let canonical = match resolve(share, &req.handle) {
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

    let content = match std::fs::read(&canonical) {
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
        if let Ok(file_meta) = std::fs::metadata(&canonical) {
            let mtime_ns = match file_meta.modified() {
                Ok(t) => t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0),
                Err(_) => 0,
            };
            let file_size = file_meta.len();
            let _ = database.put_merkle(&canonical, mtime_ns, file_size, &root, &leaf_hashes);
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
