use std::path::{Path, PathBuf};

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
use crate::handler::{
    error_detail, io_err_kind_to_code, is_within_share, resolve, verify_symlink_containment,
};

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
    // Decode and validate the request.
    let Some(req) = decode_lookup_request(payload) else {
        return lookup_error(ErrorCode::ErrorUnsupported);
    };

    // Resolve parent handle to canonical path.
    let Some(parent_canonical) = resolve_parent_path(share, &req.parent_handle, handle_db).await
    else {
        return lookup_error(ErrorCode::ErrorNotFound);
    };

    // Canonicalize the share root once.
    let share_canonical = match tokio::fs::canonicalize(share).await {
        Ok(p) => p,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    let child_path = parent_canonical.join(&req.name);

    // Get initial metadata to determine file type.
    let child_meta = match tokio::fs::symlink_metadata(&child_path).await {
        Ok(m) => m,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    let initial_is_symlink = child_meta.is_symlink();

    // Attempt symlink handling first.
    if initial_is_symlink {
        if let Some(resp) = handle_symlink_child(&child_path, &share_canonical, handle_db).await {
            return resp;
        }
        // TOCTOU detected type change — fall through to regular path.
    }

    // Regular file or directory (including fall-through from symlink TOCTOU case).
    let child_canonical = match tokio::fs::canonicalize(&child_path).await {
        Ok(p) => p,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    // Handle the regular/updated-symlink case.
    handle_regular_child(
        &child_path,
        &child_canonical,
        &share_canonical,
        initial_is_symlink,
        handle_db,
        db,
        chunker,
    )
    .await
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper functions
// ─────────────────────────────────────────────────────────────────────────────

/// Decode the lookup payload and validate name component.
/// Returns `Some(LookupRequest)` if valid, `None` on decode error or invalid name.
fn decode_lookup_request(payload: &[u8]) -> Option<LookupRequest> {
    let req = LookupRequest::decode(payload).ok()?;
    if !is_valid_name_component(&req.name) {
        return None;
    }
    Some(req)
}

/// Resolve the parent handle to its canonical path via the `HandleDatabase`.
/// Returns `Some(PathBuf)` on success, `None` if the handle is invalid or not found.
async fn resolve_parent_path(
    share: &Path,
    parent_handle: &[u8],
    handle_db: &HandleDatabase,
) -> Option<PathBuf> {
    let parent_uuid = Uuid::from_slice(parent_handle).ok()?;
    let resolved = resolve(share, &parent_uuid, handle_db).await.ok()?;
    Some(resolved.canonical)
}

/// Handle a symlink child: verify containment, TOCTOU re-check, build response.
/// Returns `Some(LookupResponse)` if confirmed symlink after all checks.
/// Returns `None` if a TOCTOU type change was detected (caller should fall through
/// to the regular file/directory path).
async fn handle_symlink_child(
    child_path: &Path,
    share_canonical: &Path,
    handle_db: &HandleDatabase,
) -> Option<LookupResponse> {
    // Read the symlink target.
    let target = tokio::fs::read_link(child_path).await.ok()?;

    // Security: verify the symlink's resolved target is within the share.
    if verify_symlink_containment(child_path, share_canonical)
        .await
        .is_none()
    {
        return Some(lookup_error(ErrorCode::ErrorNotFound));
    }

    // TOCTOU hardening: re-verify is_symlink after canonicalize.
    let current_meta = tokio::fs::symlink_metadata(child_path).await.ok()?;
    if !current_meta.is_symlink() {
        // Was a symlink, replaced by a regular file/directory — fall through.
        tracing::warn!(
            path = %child_path.display(),
            "TOCTOU: symlink was replaced by regular file between metadata checks, treating as regular file"
        );
        return None;
    }

    // Still a symlink — proceed to build the symlink response.
    Some(build_symlink_entry(child_path, &current_meta, &target, handle_db).await)
}

/// Handle a regular file or directory (or a symlink that changed type via TOCTOU).
/// Performs TOCTOU re-check when the initial type was regular and a symlink now exists.
async fn handle_regular_child<M: MerkleCache>(
    child_path: &Path,
    child_canonical: &Path,
    share_canonical: &Path,
    initial_is_symlink: bool,
    handle_db: &HandleDatabase,
    db: &M,
    chunker: Chunker,
) -> LookupResponse {
    // Check containment for the canonical path.
    if !is_within_share(child_canonical, share_canonical) {
        return lookup_error(ErrorCode::ErrorNotFound);
    }

    // TOCTOU hardening: re-verify is_symlink after canonicalize for the non-symlink path.
    if !initial_is_symlink {
        let Ok(current_meta) = tokio::fs::symlink_metadata(child_path).await else {
            tracing::warn!(
                path = %child_path.display(),
                "TOCTOU: path disappeared between metadata checks"
            );
            return lookup_error(ErrorCode::ErrorNotFound);
        };
        if current_meta.is_symlink() {
            // Regular file was replaced by a symlink. Re-do containment and build symlink response.
            tracing::warn!(
                path = %child_path.display(),
                "TOCTOU: regular file was replaced by symlink between metadata checks, treating as symlink"
            );
            let target = match tokio::fs::read_link(child_path).await {
                Ok(t) => t,
                Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
            };
            if verify_symlink_containment(child_path, share_canonical)
                .await
                .is_none()
            {
                return lookup_error(ErrorCode::ErrorNotFound);
            }
            return build_symlink_entry(child_path, &current_meta, &target, handle_db).await;
        }
    }

    // Regular file or directory: get metadata and build response.
    let meta = match tokio::fs::metadata(child_canonical).await {
        Ok(m) => m,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    build_regular_entry(child_canonical, &meta, handle_db, db, chunker).await
}

/// Build a `LookupResponse` for a symlink entry.
async fn build_symlink_entry(
    child_path: &Path,
    meta: &std::fs::Metadata,
    target: &Path,
    handle_db: &HandleDatabase,
) -> LookupResponse {
    let handle = match handle_db
        .get_or_create_handle_non_canonical(child_path)
        .await
    {
        Ok(uuid) => uuid.as_bytes().to_vec(),
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    let root_hash = sentinel_hash_for_non_file(FileType::Symlink);
    let symlink_target_bytes = target.to_string_lossy().into_owned().into_bytes();

    LookupResponse {
        result: Some(lookup_response::Result::Entry(LookupResult {
            handle,
            attrs: Some(build_attrs_with_symlink_target(
                meta,
                &root_hash,
                symlink_target_bytes,
            )),
        })),
    }
}

/// Build a `LookupResponse` for a regular file or directory entry.
async fn build_regular_entry<M: MerkleCache>(
    child_canonical: &Path,
    meta: &std::fs::Metadata,
    handle_db: &HandleDatabase,
    db: &M,
    chunker: Chunker,
) -> LookupResponse {
    let handle = match handle_db.get_or_create_handle(child_canonical).await {
        Ok(uuid) => uuid.as_bytes().to_vec(),
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    let root_hash = get_or_compute_merkle_root(child_canonical, meta, db, chunker).await;

    LookupResponse {
        result: Some(lookup_response::Result::Entry(LookupResult {
            handle,
            attrs: Some(build_attrs_with_symlink_target(meta, &root_hash, vec![])),
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
            attrs.symlink_target, b"target.txt",
            "symlink_target must match the link target"
        );

        // 3. The handle must map back to the symlink's own path, not the canonical target.
        // Use the canonical share path so the comparison works on macOS where
        // TempDir returns /var/... but canonicalize() resolves /var -> /private/var.
        let handle_uuid = Uuid::from_slice(&entry.handle).expect("handle must be a valid UUID");
        let stored_path = handle_db
            .get_path(&handle_uuid)
            .expect("handle must exist in database");
        let link_path = share.canonicalize().unwrap().join("link.txt");
        assert_eq!(
            stored_path, link_path,
            "symlink handle must map to the symlink's own path"
        );
    }

    /// A symlink whose target is outside the share must return `ErrorNotFound`.
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

    /// A broken symlink (target does not exist) must return `ErrorNotFound`.
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

    // -----------------------------------------------------------------------
    // TOCTOU re-verification tests
    //
    // These tests verify that lookup_response correctly handles type
    // changes that could occur in a TOCTOU race. While we can't reproduce
    // the exact timing of a race, we verify the observable behavior:
    // if a symlink is replaced by a regular file (or vice versa),
    // lookup returns the correct type for the current filesystem state.
    // -----------------------------------------------------------------------

    /// When a symlink is replaced by a regular file between two lookup calls,
    /// the second lookup must return `FileType::Regular`, not <File::Symlink>.
    /// This verifies the TOCTOU re-verification: if the type changes after
    /// the initial `symlink_metadata` check, the response reflects reality.
    #[tokio::test]
    #[cfg(unix)]
    async fn lookup_response_symlink_replaced_by_file_returns_regular_type() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Start with a symlink pointing to a regular file.
        std::fs::write(share.join("target.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("target.txt", share.join("entry")).unwrap();

        let handle_db = HandleDatabase::new();
        let parent_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        // First lookup: should see a symlink.
        let req = LookupRequest {
            parent_handle: parent_uuid.as_bytes().to_vec(),
            name: "entry".to_string(),
        };
        let payload = req.encode_to_vec();

        let resp =
            lookup_response(&payload, &share, &NoopCache, &handle_db, Chunker::default()).await;

        let entry = match resp.result {
            Some(lookup_response::Result::Entry(e)) => e,
            other => panic!("expected Entry for symlink lookup, got: {:?}", other),
        };
        let attrs = entry.attrs.expect("attrs must be present");
        assert_eq!(
            attrs.file_type,
            FileType::Symlink as i32,
            "initial lookup must return FileType::Symlink"
        );

        // Now replace the symlink with a regular file (simulates TOCTOU race).
        std::fs::remove_file(share.join("entry")).unwrap();
        std::fs::write(share.join("entry"), b"replaced").unwrap();

        // Second lookup: must return FileType::Regular.
        // Without TOCTOU re-verification, the code might cache the symlink
        // type from the first symlink_metadata call and return stale data.
        // With re-verification, it detects the type change and returns Regular.
        let resp2 =
            lookup_response(&payload, &share, &NoopCache, &handle_db, Chunker::default()).await;

        let entry2 = match resp2.result {
            Some(lookup_response::Result::Entry(e)) => e,
            other => panic!("expected Entry after file-swap, got: {:?}", other),
        };
        let attrs2 = entry2.attrs.expect("attrs must be present");
        assert_eq!(
            attrs2.file_type,
            FileType::Regular as i32,
            "after symlink→file swap, lookup must return FileType::Regular"
        );
        // symlink_target must be empty for a regular file.
        assert!(
            attrs2.symlink_target.is_empty(),
            "regular file must not have symlink_target"
        );
    }

    /// When a regular file is replaced by a symlink between two lookup calls,
    /// the second lookup must return `FileType::Symlink` with the correct target.
    #[tokio::test]
    #[cfg(unix)]
    async fn lookup_response_file_replaced_by_symlink_returns_symlink_type() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Start with a regular file.
        std::fs::write(share.join("target.txt"), b"hello").unwrap();
        std::fs::write(share.join("entry"), b"data").unwrap();

        let handle_db = HandleDatabase::new();
        let parent_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        // First lookup: should see a regular file.
        let req = LookupRequest {
            parent_handle: parent_uuid.as_bytes().to_vec(),
            name: "entry".to_string(),
        };
        let payload = req.encode_to_vec();

        let resp =
            lookup_response(&payload, &share, &NoopCache, &handle_db, Chunker::default()).await;

        let entry = match resp.result {
            Some(lookup_response::Result::Entry(e)) => e,
            other => panic!("expected Entry for regular file lookup, got: {:?}", other),
        };
        let attrs = entry.attrs.expect("attrs must be present");
        assert_eq!(
            attrs.file_type,
            FileType::Regular as i32,
            "initial lookup must return FileType::Regular"
        );

        // Replace the regular file with a symlink.
        std::fs::remove_file(share.join("entry")).unwrap();
        std::os::unix::fs::symlink("target.txt", share.join("entry")).unwrap();

        // Second lookup: must return FileType::Symlink.
        let resp2 =
            lookup_response(&payload, &share, &NoopCache, &handle_db, Chunker::default()).await;

        let entry2 = match resp2.result {
            Some(lookup_response::Result::Entry(e)) => e,
            other => panic!("expected Entry after symlink swap, got: {:?}", other),
        };
        let attrs2 = entry2.attrs.expect("attrs must be present");
        assert_eq!(
            attrs2.file_type,
            FileType::Symlink as i32,
            "after file→symlink swap, lookup must return FileType::Symlink"
        );
        assert_eq!(
            attrs2.symlink_target, b"target.txt",
            "symlink_target must be the link target"
        );
    }

    /// When the entry at a name disappears between the parent resolution and
    /// the child metadata check, lookup must return `ErrorNotFound`.
    #[tokio::test]
    async fn lookup_response_disappearing_entry_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Create a regular file.
        std::fs::write(share.join("ephemeral.txt"), b"gone soon").unwrap();

        let handle_db = HandleDatabase::new();
        let parent_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        // Build the lookup request while the file exists.
        let req = LookupRequest {
            parent_handle: parent_uuid.as_bytes().to_vec(),
            name: "ephemeral.txt".to_string(),
        };
        let payload = req.encode_to_vec();

        // Delete the file before calling lookup.
        std::fs::remove_file(share.join("ephemeral.txt")).unwrap();

        let resp =
            lookup_response(&payload, &share, &NoopCache, &handle_db, Chunker::default()).await;

        match resp.result {
            Some(lookup_response::Result::Error(err)) => {
                assert_eq!(
                    err.code,
                    ErrorCode::ErrorNotFound as i32,
                    "disappeared entry must return ErrorNotFound"
                );
            }
            other => panic!("expected Error for disappeared entry, got: {:?}", other),
        }
    }
}
