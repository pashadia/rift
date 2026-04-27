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
    // If it is, read the target and canonicalize for the security containment check.
    // Then re-verify is_symlink to close the TOCTOU window between the initial
    // symlink_metadata and canonicalize.
    let child_meta = match tokio::fs::symlink_metadata(&child_path).await {
        Ok(m) => m,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    let initial_is_symlink = child_meta.is_symlink();

    // For symlinks, read the target and canonicalize for containment check.
    // The containment check ensures symlink targets outside the share are rejected.
    if initial_is_symlink {
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
        if verify_symlink_containment(&child_path, &share_canonical)
            .await
            .is_none()
        {
            return lookup_error(ErrorCode::ErrorNotFound);
        }

        // TOCTOU hardening: re-verify is_symlink after canonicalize.
        // Between the initial symlink_metadata and canonicalize, the filesystem
        // could change (symlink replaced by regular file or vice versa).
        let current_meta = match tokio::fs::symlink_metadata(&child_path).await {
            Ok(m) => m,
            Err(_) => {
                tracing::warn!(
                    path = %child_path.display(),
                    "TOCTOU: path disappeared between metadata checks"
                );
                return lookup_error(ErrorCode::ErrorNotFound);
            }
        };

        if current_meta.is_symlink() {
            // Still a symlink — proceed as intended.
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
                        &current_meta,
                        root_hash,
                        symlink_target_str,
                    )),
                })),
            };
        } else {
            // Was a symlink, replaced by a regular file/directory between
            // the initial symlink_metadata and canonicalize. Treat it as a
            // regular file from now on — fall through to the non-symlink path.
            tracing::warn!(
                path = %child_path.display(),
                "TOCTOU: symlink was replaced by regular file between metadata checks, treating as regular file"
            );
            // Fall through to non-symlink path below.
        }
    }

    // --- Non-symlink path (regular file or directory) ---
    // Also reached via fall-through when a symlink was replaced by a regular file.
    let child_canonical = match tokio::fs::canonicalize(&child_path).await {
        Ok(p) => p,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    let symlink_out_of_the_share = !is_within_share(&child_canonical, &share_canonical);
    if symlink_out_of_the_share {
        return lookup_error(ErrorCode::ErrorNotFound);
    }

    // TOCTOU hardening: re-verify is_symlink after canonicalize for the
    // non-symlink path. Between symlink_metadata and canonicalize, a regular
    // file could have been replaced by a symlink.
    if !initial_is_symlink {
        let current_meta = match tokio::fs::symlink_metadata(&child_path).await {
            Ok(m) => m,
            Err(_) => {
                tracing::warn!(
                    path = %child_path.display(),
                    "TOCTOU: path disappeared between metadata checks"
                );
                return lookup_error(ErrorCode::ErrorNotFound);
            }
        };
        if current_meta.is_symlink() {
            // Regular file was replaced by a symlink. Treat it as a symlink now.
            tracing::warn!(
                path = %child_path.display(),
                "TOCTOU: regular file was replaced by symlink between metadata checks, treating as symlink"
            );
            let target = match tokio::fs::read_link(&child_path).await {
                Ok(t) => t,
                Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
            };
            // Redo containment check through the new symlink.
            if verify_symlink_containment(&child_path, &share_canonical)
                .await
                .is_none()
            {
                return lookup_error(ErrorCode::ErrorNotFound);
            }

            let handle = match handle_db
                .get_or_create_handle_non_canonical(&child_path)
                .await
            {
                Ok(uuid) => uuid.as_bytes().to_vec(),
                Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
            };

            let root_hash = sentinel_hash_for_non_file(FileType::Symlink);

            return LookupResponse {
                result: Some(lookup_response::Result::Entry(LookupResult {
                    handle,
                    attrs: Some(build_attrs_with_symlink_target(
                        &current_meta,
                        root_hash,
                        target.to_string_lossy().into_owned(),
                    )),
                })),
            };
        }
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
    /// the second lookup must return FileType::Regular, not File::Symlink.
    /// This verifies the TOCTOU re-verification: if the type changes after
    /// the initial symlink_metadata check, the response reflects reality.
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
    /// the second lookup must return FileType::Symlink with the correct target.
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
            attrs2.symlink_target, "target.txt",
            "symlink_target must be the link target"
        );
    }

    /// When the entry at a name disappears between the parent resolution and
    /// the child metadata check, lookup must return ErrorNotFound.
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
