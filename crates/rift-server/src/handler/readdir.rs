use std::path::Path;

use prost::Message as _;
use tracing::instrument;

use rift_protocol::messages::{
    readdir_response, ErrorCode, FileType, ReaddirEntry, ReaddirRequest, ReaddirResponse,
    ReaddirSuccess,
};

use uuid::Uuid;

use crate::handle::HandleDatabase;
use crate::handler::{error_detail, io_err_kind_to_code, resolve};

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

    let dir_uuid = match Uuid::from_slice(&req.directory_handle) {
        Ok(u) => u,
        Err(_) => return readdir_error(ErrorCode::ErrorNotFound),
    };

    let dir_canonical = match resolve(share, &dir_uuid, handle_db).await {
        Ok(r) => r.canonical,
        Err(e) => {
            let code = e
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .map(|io| io_err_kind_to_code(io.kind()))
                .unwrap_or(ErrorCode::ErrorNotFound);
            return readdir_error(code);
        }
    };

    use tokio_stream::wrappers::ReadDirStream;
    use tokio_stream::StreamExt;

    let entries: Vec<ReaddirEntry> = match tokio::fs::read_dir(&dir_canonical).await {
        Ok(read_dir) => {
            let stream = ReadDirStream::new(read_dir);
            let share_canonical = tokio::fs::canonicalize(share)
                .await
                .ok()
                .unwrap_or_else(|| share.to_path_buf());
            let entries_with_none: Vec<Option<ReaddirEntry>> = stream
                .then(|entry_result| {
                    let share_canonical = share_canonical.clone();
                    let dir_display = dir_canonical.display().to_string();
                    async move {
                        let entry = match entry_result {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::warn!(path = %dir_display, error = %e, "readdir: failed to read directory entry");
                                return None;
                            }
                        };
                        let file_type = match entry.file_type().await {
                            Ok(ft) => ft,
                            Err(e) => {
                                tracing::warn!(path = %entry.path().display(), error = %e, "readdir: failed to get file type");
                                return None;
                            }
                        };
                        let proto_type = if file_type.is_dir() {
                            FileType::Directory as i32
                        } else if file_type.is_symlink() {
                            FileType::Symlink as i32
                        } else {
                            FileType::Regular as i32
                        };
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let entry_path = entry.path();

                        // For symlinks: use the symlink's own path (not canonical) for the handle
                        // and read the symlink target. For everything else: canonicalize as before.
                        // Then check that the canonical path is within the share root.
                        let (handle, symlink_target) = if file_type.is_symlink() {
                            let target = match tokio::fs::read_link(&entry_path).await {
                                Ok(t) => t,
                                Err(e) => {
                                    tracing::warn!(path = %entry_path.display(), error = %e, "readdir: failed to read symlink target");
                                    return None;
                                }
                            };
                            // Validate that the symlink target, when resolved, is within the share.
                            // This filters out symlinks pointing outside the share and broken symlinks.
                            // Note: broken symlink canonicalization failure is kept silent — it's
                            // expected filtering, not an error condition.
                            let canonical = match tokio::fs::canonicalize(&entry_path).await {
                                Ok(p) => p,
                                Err(_) => return None, // broken symlink or inaccessible — kept silent per design
                            };
                            if !canonical.starts_with(&share_canonical) {
                                return None; // symlink target outside share — intentional security filtering, kept silent
                            }
                            // Use the symlink's own path for the handle, not the canonical target.
                            let uuid = handle_db
                                .get_or_create_handle_non_canonical(&entry_path)
                                .await
                                .ok()?;
                            let handle = uuid.as_bytes().to_vec();
                            (handle, Some(target.to_string_lossy().into_owned()))
                        } else {
                            let entry_canonical = match tokio::fs::canonicalize(&entry_path).await {
                                Ok(p) => p,
                                Err(e) => {
                                    tracing::warn!(path = %entry_path.display(), error = %e, "readdir: failed to canonicalize entry");
                                    return None;
                                }
                            };
                            let handle = handle_db
                                .get_or_create_handle(&entry_canonical)
                                .await
                                .ok()?
                                .as_bytes()
                                .to_vec();
                            (handle, None)
                        };

                        Some(ReaddirEntry {
                            name,
                            file_type: proto_type,
                            handle,
                            symlink_target: symlink_target.unwrap_or_default(),
                        })
                    }
                })
                .collect()
                .await;
            entries_with_none.into_iter().flatten().collect()
        }
        Err(e) => return readdir_error(io_err_kind_to_code(e.kind())),
    };

    let mut entries = entries;
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let offset = req.offset as usize;
    let entries: Vec<ReaddirEntry> = entries.into_iter().skip(offset).collect();

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

fn readdir_error(code: ErrorCode) -> ReaddirResponse {
    ReaddirResponse {
        result: Some(readdir_response::Result::Error(error_detail(code))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use tempfile::TempDir;

    use crate::handle::HandleDatabase;

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
                let names: Vec<&str> = success.entries.iter().map(|e| e.name.as_str()).collect();
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

        match resp.result {
            Some(readdir_response::Result::Error(_)) => {}
            other => panic!("expected Error for garbage payload, got: {:?}", other),
        }
    }

    /// Symlinks within the share must:
    ///   1. Report `FileType::Symlink`.
    ///   2. Include the symlink target in `symlink_target`.
    ///   3. Use the symlink's OWN path (not the canonical target) for the handle.
    #[tokio::test]
    async fn readdir_response_symlink_uses_own_path_and_includes_target() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Create a regular file and a symlink pointing to it.
        std::fs::write(share.join("target.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("target.txt", share.join("link.txt")).unwrap();

        let handle_db = HandleDatabase::new();
        let dir_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = ReaddirRequest {
            directory_handle: dir_uuid.as_bytes().to_vec(),
            offset: 0,
            limit: 0,
        };
        let payload = req.encode_to_vec();

        let resp = readdir_response(&payload, &share, &handle_db).await;

        let success = match resp.result {
            Some(readdir_response::Result::Entries(s)) => s,
            other => panic!("expected Entries, got: {:?}", other),
        };

        let link_entry = success
            .entries
            .iter()
            .find(|e| e.name == "link.txt")
            .expect("must list link.txt");

        // 1. File type must be Symlink
        assert_eq!(
            link_entry.file_type,
            FileType::Symlink as i32,
            "link.txt must have FileType::Symlink"
        );

        // 2. symlink_target must be set and match the expected target
        assert_eq!(
            link_entry.symlink_target, "target.txt",
            "symlink_target must match the link target"
        );

        // 3. The handle must have been created from the symlink's own path,
        //    not from the canonical target path. Look up the handle in the
        //    database and verify it resolves back to the symlink's own path
        //    (not the resolved target).
        let link_path = share.join("link.txt");
        let handle_uuid =
            Uuid::from_slice(&link_entry.handle).expect("handle must be a valid UUID");
        let stored_path = handle_db
            .get_path(&handle_uuid)
            .expect("handle must exist in database");
        assert_eq!(
            stored_path, link_path,
            "symlink handle must map to the symlink's own path, not the canonical target"
        );
    }

    /// When a directory contains multiple symlinks, every one must report the
    /// correct `file_type` and `symlink_target`. This also serves as a regression
    /// guard for the share_canonical hoist: if the hoist were broken, entries
    /// could silently vanish or report wrong metadata.
    #[tokio::test]
    async fn readdir_response_multiple_symlinks_all_consistent() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Create 3 regular files and 3 symlinks pointing to them.
        for i in 0..3 {
            let file_name = format!("file{i}.txt");
            let link_name = format!("link{i}.txt");
            std::fs::write(share.join(&file_name), b"data").unwrap();
            std::os::unix::fs::symlink(&file_name, share.join(&link_name)).unwrap();
        }

        let handle_db = HandleDatabase::new();
        let dir_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = ReaddirRequest {
            directory_handle: dir_uuid.as_bytes().to_vec(),
            offset: 0,
            limit: 0,
        };
        let payload = req.encode_to_vec();

        let resp = readdir_response(&payload, &share, &handle_db).await;

        let success = match resp.result {
            Some(readdir_response::Result::Entries(s)) => s,
            other => panic!("expected Entries, got: {:?}", other),
        };

        // All 6 entries (3 files + 3 symlinks) must be present.
        assert_eq!(
            success.entries.len(),
            6,
            "expected exactly 6 entries (3 files + 3 symlinks)"
        );

        for i in 0..3 {
            let link_name = format!("link{i}.txt");
            let target_name = format!("file{i}.txt");
            let entry = success
                .entries
                .iter()
                .find(|e| e.name == link_name)
                .unwrap_or_else(|| panic!("must list {link_name}"));
            assert_eq!(
                entry.file_type,
                FileType::Symlink as i32,
                "{link_name} must have FileType::Symlink"
            );
            assert_eq!(
                entry.symlink_target, target_name,
                "{link_name} symlink_target must be {target_name}"
            );
        }
    }

    /// Symlinks whose canonical target is outside the share must be filtered out.
    #[tokio::test]
    async fn readdir_response_filters_symlink_pointing_outside_share() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Create a symlink pointing to an absolute path outside the share.
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), share.join("outside_link")).unwrap();

        // Also create a regular file so the directory isn't empty.
        std::fs::write(share.join("regular.txt"), b"data").unwrap();

        let handle_db = HandleDatabase::new();
        let dir_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = ReaddirRequest {
            directory_handle: dir_uuid.as_bytes().to_vec(),
            offset: 0,
            limit: 0,
        };
        let payload = req.encode_to_vec();

        let resp = readdir_response(&payload, &share, &handle_db).await;

        let success = match resp.result {
            Some(readdir_response::Result::Entries(s)) => s,
            other => panic!("expected Entries, got: {:?}", other),
        };

        let names: Vec<&str> = success.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            !names.contains(&"outside_link"),
            "symlink pointing outside the share must be filtered out"
        );
        assert!(
            names.contains(&"regular.txt"),
            "regular files must still be listed"
        );
    }

    /// When a directory entry fails `canonicalize` due to a permission error,
    /// the readdir must still succeed with the remaining entries (not fail
    /// entirely). The permission-denied entry is skipped — the readdir protocol
    /// has no per-entry error mechanism — but `tracing::warn` should be emitted.
    #[tokio::test]
    #[cfg(unix)]
    async fn readdir_response_succeeds_with_remaining_entries_on_permission_denied() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Create 3 files
        std::fs::write(share.join("alpha.txt"), b"a").unwrap();
        std::fs::write(share.join("beta.txt"), b"b").unwrap();
        std::fs::write(share.join("gamma.txt"), b"c").unwrap();

        // Make beta.txt unreadable — canonicalize will fail with PermissionDenied
        std::fs::set_permissions(
            share.join("beta.txt"),
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .unwrap();

        let handle_db = HandleDatabase::new();
        let dir_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = ReaddirRequest {
            directory_handle: dir_uuid.as_bytes().to_vec(),
            offset: 0,
            limit: 0,
        };
        let payload = prost::Message::encode_to_vec(&req);

        let resp = readdir_response(&payload, &share, &handle_db).await;

        let success = match resp.result {
            Some(readdir_response::Result::Entries(s)) => s,
            other => panic!("expected Entries, got: {:?}", other),
        };

        let names: Vec<&str> = success.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"alpha.txt"),
            "alpha.txt must be listed, got {names:?}"
        );
        assert!(
            names.contains(&"gamma.txt"),
            "gamma.txt must be listed, got {names:?}"
        );
        // beta.txt is skipped because canonicalize fails — that's correct

        // Restore permissions so TempDir cleanup can succeed
        std::fs::set_permissions(
            share.join("beta.txt"),
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .unwrap();
    }
}
