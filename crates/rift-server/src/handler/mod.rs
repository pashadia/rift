//! Pure request-handler functions for the Rift server.
//!
//! Each `*_response` function decodes a proto request from raw bytes,
//! performs the filesystem work using async I/O, and returns a proto
//! response.  The async dispatch layer in `server.rs` handles the
//! transport and calls these functions.
//!
//! All handlers validate raw handle bytes as UUIDs at the network boundary
//! before any filesystem access, rejecting malformed handles immediately.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use tracing::instrument;

use uuid::Uuid;

use crate::handle::HandleDatabase;

pub mod attrs;
pub mod drill;
pub mod lookup;
pub mod merkle_cache;
pub mod merkle_cache_trait;
pub mod read;
pub mod readdir;
pub mod stat;

pub use attrs::{build_attrs, metadata_to_attrs};
pub use drill::merkle_drill_response;
pub use lookup::lookup_response;
pub use merkle_cache_trait::{MerkleCache, NoopCache};
pub use read::{read_response, MAX_CHUNK_COUNT};
pub use readdir::readdir_response;
pub use stat::stat_response;

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

pub(crate) fn error_detail(
    code: rift_protocol::messages::ErrorCode,
) -> rift_protocol::messages::ErrorDetail {
    rift_protocol::messages::ErrorDetail {
        code: code as i32,
        message: code.as_str_name().to_string(),
        metadata: None,
    }
}

pub(crate) fn io_err_kind_to_code(kind: std::io::ErrorKind) -> rift_protocol::messages::ErrorCode {
    use rift_protocol::messages::ErrorCode;
    match kind {
        std::io::ErrorKind::NotFound => ErrorCode::ErrorNotFound,
        std::io::ErrorKind::PermissionDenied => ErrorCode::ErrorPermissionDenied,
        _ => ErrorCode::ErrorNotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    use crate::handle::HandleDatabase;

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
    async fn resolve_unknown_uuid_returns_error() {
        let tmp = TempDir::new().unwrap();
        let handle_db = HandleDatabase::new();
        let unknown = Uuid::from_bytes([0x42; 16]);
        let result = resolve(tmp.path(), &unknown, &handle_db).await;
        assert!(result.is_err(), "unknown UUID must produce an error");
    }
}
