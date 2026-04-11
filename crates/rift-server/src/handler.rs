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

use rift_protocol::messages::{
    lookup_response, readdir_response, stat_result, ErrorCode, ErrorDetail, FileAttrs, FileType,
    LookupRequest, LookupResponse, LookupResult, ReaddirEntry, ReaddirRequest, ReaddirResponse,
    ReaddirSuccess, StatRequest, StatResponse, StatResult,
};

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

/// Convert `std::fs::Metadata` to a proto `FileAttrs` message.
///
/// Uses Unix-specific metadata fields (`mode`, `uid`, `gid`, `nlink`).
pub fn metadata_to_attrs(meta: &std::fs::Metadata) -> FileAttrs {
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
    }
}

// ---------------------------------------------------------------------------
// Request handlers (pure: decode → filesystem → encode)
// ---------------------------------------------------------------------------

/// Handle a `StatRequest`: stat each requested handle and return one
/// `StatResult` per handle (success or error).
///
/// Malformed payloads return an empty result list rather than panicking.
#[instrument(skip(share), fields(share = %share.display()), level = "debug")]
pub fn stat_response(payload: &[u8], share: &Path) -> StatResponse {
    let req = match StatRequest::decode(payload) {
        Ok(r) => r,
        Err(_) => return StatResponse { results: vec![] },
    };

    let results = req
        .handles
        .into_iter()
        .map(|handle| {
            match resolve(share, &handle)
                .and_then(|p| std::fs::metadata(&p).map_err(anyhow::Error::from))
            {
                Ok(meta) => StatResult {
                    result: Some(stat_result::Result::Attrs(metadata_to_attrs(&meta))),
                },
                Err(_) => stat_error(io_error_code(&handle, share)),
            }
        })
        .collect();

    StatResponse { results }
}

/// Handle a `LookupRequest`: resolve `(parent_handle, name)` to a child
/// handle and its attributes.
///
/// Returns `ErrorNotFound` if either the parent or the child does not exist.
#[instrument(skip(share), fields(share = %share.display()), level = "debug")]
pub fn lookup_response(payload: &[u8], share: &Path) -> LookupResponse {
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

    // Security: child must still be within the share after symlink resolution.
    if !child_canonical.starts_with(&share_canonical) {
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

    LookupResponse {
        result: Some(lookup_response::Result::Entry(LookupResult {
            handle,
            attrs: Some(metadata_to_attrs(&meta)),
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

fn io_err_kind_to_code(kind: std::io::ErrorKind) -> ErrorCode {
    match kind {
        std::io::ErrorKind::NotFound => ErrorCode::ErrorNotFound,
        std::io::ErrorKind::PermissionDenied => ErrorCode::ErrorPermissionDenied,
        _ => ErrorCode::ErrorNotFound,
    }
}
