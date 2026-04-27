use std::path::Path;

use prost::Message as _;
use tracing::instrument;

use rift_common::crypto::Chunker;
use rift_protocol::messages::{
    lookup_response, ErrorCode, FileType, LookupRequest, LookupResponse, LookupResult,
};

use uuid::Uuid;

use crate::handle::HandleDatabase;
use crate::handler::attrs::build_attrs_with_symlink_target;
use crate::handler::merkle_cache::{get_or_compute_merkle_root, sentinel_hash_for_non_file};
use crate::handler::merkle_cache_trait::MerkleCache;
use crate::handler::{error_detail, io_err_kind_to_code, resolve};

/// Handle a `LookupRequest`: resolve `(parent_handle, name)` to a child
/// handle and its attributes.
///
/// Returns `ErrorNotFound` if either the parent or the child does not exist.
/// Name components must be single path elements (no `/` or NUL).
#[instrument(skip(share, db, handle_db), fields(share = %share.display()), level = "debug")]
pub async fn lookup_response<M: MerkleCache>(
    payload: &[u8],
    share: &Path,
    db: &M,
    handle_db: &HandleDatabase,
    chunker: Chunker,
) -> LookupResponse {
    let req = match LookupRequest::decode(payload) {
        Ok(r) => r,
        Err(_) => return lookup_error(ErrorCode::ErrorUnsupported),
    };

    if !is_valid_name_component(&req.name) {
        return lookup_error(ErrorCode::ErrorUnsupported);
    }

    let parent_uuid = match Uuid::from_slice(&req.parent_handle) {
        Ok(u) => u,
        Err(_) => return lookup_error(ErrorCode::ErrorNotFound),
    };

    let parent_canonical = match resolve(share, &parent_uuid, handle_db).await {
        Ok(r) => r.canonical,
        Err(_) => return lookup_error(ErrorCode::ErrorNotFound),
    };

    let share_canonical = match tokio::fs::canonicalize(share).await {
        Ok(p) => p,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    let child_path = parent_canonical.join(&req.name);

    // --- Symlink handling ---
    // Check if the child is a symlink using symlink_metadata (doesn't follow links).
    // If it is, return FileType::Symlink with the symlink's own path for the handle,
    // not the resolved target's path.
    let child_meta = match tokio::fs::symlink_metadata(&child_path).await {
        Ok(m) => m,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    if child_meta.is_symlink() {
        // Read the symlink target.
        let target = match tokio::fs::read_link(&child_path).await {
            Ok(t) => t,
            Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
        };

        // Security: verify the symlink's resolved target is within the share.
        //
        // NOTE: The containment check uses `canonicalize()` which resolves `..`
        // and symlinks via the filesystem. For non-broken symlinks, this is safe
        // because `canonicalize` returns the fully-resolved absolute path.
        // Broken symlinks (target doesn't exist) will fail canonicalize and
        // return ErrorNotFound, making them invisible through the mount.
        //
        // Broken symlinks that escape via `..` in their relative or absolute
        // target are handled by the `resolve()` function in mod.rs, which
        // normalizes `..` in symlink targets before checking containment.
        let child_canonical = match tokio::fs::canonicalize(&child_path).await {
            Ok(p) => p,
            Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
        };
        if !child_canonical.starts_with(&share_canonical) {
            return lookup_error(ErrorCode::ErrorNotFound);
        }

        // Handle is for the symlink path itself, not the target.
        let handle = match handle_db
            .get_or_create_handle_non_canonical(&child_path)
            .await
        {
            Ok(uuid) => uuid.as_bytes().to_vec(),
            Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
        };

        let root_hash = sentinel_hash_for_non_file(FileType::Symlink);
        let symlink_target_str = target.to_string_lossy().into_owned();

        return LookupResponse {
            result: Some(lookup_response::Result::Entry(LookupResult {
                handle,
                attrs: Some(build_attrs_with_symlink_target(
                    &child_meta,
                    root_hash,
                    symlink_target_str,
                )),
            })),
        };
    }

    // --- Non-symlink path (regular file or directory) ---
    let child_canonical = match tokio::fs::canonicalize(&child_path).await {
        Ok(p) => p,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
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
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    let root_hash = get_or_compute_merkle_root(&child_canonical, &meta, db, chunker).await;

    LookupResponse {
        result: Some(lookup_response::Result::Entry(LookupResult {
            handle,
            attrs: Some(build_attrs_with_symlink_target(
                &meta,
                root_hash,
                String::new(),
            )),
        })),
    }
}

fn lookup_error(code: ErrorCode) -> LookupResponse {
    LookupResponse {
        result: Some(lookup_response::Result::Error(error_detail(code))),
    }
}

/// Check that a lookup name is a single path component (non-empty, no `/`, no NUL).
fn is_valid_name_component(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use tempfile::TempDir;

    use crate::handle::HandleDatabase;
    use crate::handler::merkle_cache_trait::NoopCache;
    use rift_common::crypto::Chunker;
    use rift_protocol::messages::{FileType, LookupRequest};

    #[tokio::test]
    async fn lookup_response_existing_entry_returns_handle() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let child = share.join("child.txt");
        std::fs::write(&child, b"data").unwrap();

        let handle_db = HandleDatabase::new();
        let parent_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = LookupRequest {
            parent_handle: parent_uuid.as_bytes().to_vec(),
            name: "child.txt".to_string(),
        };
        let payload = req.encode_to_vec();

        let resp =
            lookup_response(&payload, &share, &NoopCache, &handle_db, Chunker::default()).await;

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

        let resp =
            lookup_response(&payload, &share, &NoopCache, &handle_db, Chunker::default()).await;

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
        let resp =
            lookup_response(&garbage, &share, &NoopCache, &handle_db, Chunker::default()).await;

        match resp.result {
            Some(lookup_response::Result::Error(_)) => {}
            other => panic!("expected Error for garbage payload, got: {:?}", other),
        }
    }

    /// Symlinks within the share must:
    ///   1. Report `FileType::Symlink` in the response attrs.
    ///   2. Include the symlink target in `symlink_target`.
    ///   3. Use the symlink's OWN path (not the canonical target) for the handle.
    #[tokio::test]
    async fn lookup_response_symlink_returns_symlink_type_and_target() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Create a regular file and a symlink pointing to it.
        std::fs::write(share.join("target.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("target.txt", share.join("link.txt")).unwrap();

        let handle_db = HandleDatabase::new();
        let parent_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = LookupRequest {
            parent_handle: parent_uuid.as_bytes().to_vec(),
            name: "link.txt".to_string(),
        };
        let payload = req.encode_to_vec();

        let resp =
            lookup_response(&payload, &share, &NoopCache, &handle_db, Chunker::default()).await;

        let entry = match resp.result {
            Some(lookup_response::Result::Entry(e)) => e,
            other => panic!("expected Entry for symlink lookup, got: {:?}", other),
        };

        let attrs = entry.attrs.expect("attrs must be present");

        // 1. File type must be Symlink
        assert_eq!(
            attrs.file_type,
            FileType::Symlink as i32,
            "symlink must have FileType::Symlink, got {:?}",
            attrs.file_type
        );

        // 2. symlink_target must be set and match the expected target
        assert_eq!(
            attrs.symlink_target, "target.txt",
            "symlink_target must match the link target"
        );

        // 3. The handle must map back to the symlink's own path, not the canonical target
        let handle_uuid = Uuid::from_slice(&entry.handle).expect("handle must be a valid UUID");
        let stored_path = handle_db
            .get_path(&handle_uuid)
            .expect("handle must exist in database");
        let link_path = share.join("link.txt");
        assert_eq!(
            stored_path, link_path,
            "symlink handle must map to the symlink's own path"
        );
    }

    /// A symlink whose target is outside the share must return ErrorNotFound.
    #[tokio::test]
    async fn lookup_response_symlink_outside_share_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Create a symlink pointing outside the share.
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), share.join("outside_link")).unwrap();

        let handle_db = HandleDatabase::new();
        let parent_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = LookupRequest {
            parent_handle: parent_uuid.as_bytes().to_vec(),
            name: "outside_link".to_string(),
        };
        let payload = req.encode_to_vec();

        let resp =
            lookup_response(&payload, &share, &NoopCache, &handle_db, Chunker::default()).await;

        match resp.result {
            Some(lookup_response::Result::Error(err)) => {
                assert_eq!(
                    err.code,
                    ErrorCode::ErrorNotFound as i32,
                    "symlink outside share must return ErrorNotFound"
                );
            }
            other => panic!("expected Error for out-of-share symlink, got: {:?}", other),
        }
    }

    /// A broken symlink (target does not exist) must return ErrorNotFound.
    #[tokio::test]
    async fn lookup_response_broken_symlink_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Create a symlink pointing to a non-existent target.
        std::os::unix::fs::symlink("nonexistent_target", share.join("broken_link")).unwrap();

        let handle_db = HandleDatabase::new();
        let parent_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = LookupRequest {
            parent_handle: parent_uuid.as_bytes().to_vec(),
            name: "broken_link".to_string(),
        };
        let payload = req.encode_to_vec();

        let resp =
            lookup_response(&payload, &share, &NoopCache, &handle_db, Chunker::default()).await;

        match resp.result {
            Some(lookup_response::Result::Error(err)) => {
                assert_eq!(
                    err.code,
                    ErrorCode::ErrorNotFound as i32,
                    "broken symlink must return ErrorNotFound"
                );
            }
            other => panic!("expected Error for broken symlink, got: {:?}", other),
        }
    }
}
