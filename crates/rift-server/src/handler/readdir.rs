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
                    async move {
                        let entry = entry_result.ok()?;
                        let file_type = entry.file_type().await.ok()?;
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let entry_path = entry.path();

                        let initial_is_symlink = file_type.is_symlink();

                        // For symlinks: read the target and canonicalize for
                        // the security containment check, then re-verify
                        // is_symlink to close the TOCTOU window.
                        // For everything else: canonicalize as before, then
                        // re-verify to catch file→symlink races.
                        let (handle, symlink_target, final_is_symlink) = if initial_is_symlink {
                            let target = tokio::fs::read_link(&entry_path).await.ok()?;
                            // Validate that the symlink target, when resolved, is within the share.
                            // This filters out symlinks pointing outside the share and broken symlinks.
                            let canonical = match tokio::fs::canonicalize(&entry_path).await {
                                Ok(p) => p,
                                Err(_) => return None, // broken symlink or inaccessible
                            };
                            if !canonical.starts_with(&share_canonical) {
                                return None; // symlink target outside share
                            }

                            // TOCTOU hardening: re-verify is_symlink after canonicalize.
                            // Between the initial file_type check and canonicalize,
                            // the filesystem could change (symlink replaced by
                            // regular file or vice versa).
                            let current_meta = match tokio::fs::symlink_metadata(&entry_path).await {
                                Ok(m) => m,
                                Err(_) => {
                                    tracing::warn!(
                                        path = %entry_path.display(),
                                        "TOCTOU: path disappeared between metadata checks"
                                    );
                                    return None;
                                }
                            };

                            if current_meta.is_symlink() {
                                // Still a symlink — proceed as intended.
                                // Use the symlink's own path for the handle, not the canonical target.
                                let uuid = handle_db
                                    .get_or_create_handle_non_canonical(&entry_path)
                                    .await
                                    .ok()?;
                                let handle = uuid.as_bytes().to_vec();
                                (handle, Some(target.to_string_lossy().into_owned()), true)
                            } else {
                                // Was a symlink, replaced by a regular file/directory.
                                // Treat it as a regular file — re-canonicalize and get
                                // a regular file handle.
                                tracing::warn!(
                                    path = %entry_path.display(),
                                    "TOCTOU: symlink was replaced by regular file between metadata checks, treating as regular file"
                                );
                                let canonical = match tokio::fs::canonicalize(&entry_path).await {
                                    Ok(p) => p,
                                    Err(_) => return None,
                                };
                                if !canonical.starts_with(&share_canonical) {
                                    return None;
                                }
                                let uuid = handle_db
                                    .get_or_create_handle(&canonical)
                                    .await
                                    .ok()?;
                                (uuid.as_bytes().to_vec(), None, false)
                            }
                        } else {
                            // Non-symlink path: canonicalize for containment check.
                            let entry_canonical = match tokio::fs::canonicalize(&entry_path).await {
                                Ok(p) => p,
                                Err(_) => return None,
                            };

                            // TOCTOU hardening: re-verify is_symlink after canonicalize.
                            // A regular file could have been replaced by a symlink.
                            let current_meta = match tokio::fs::symlink_metadata(&entry_path).await {
                                Ok(m) => m,
                                Err(_) => {
                                    tracing::warn!(
                                        path = %entry_path.display(),
                                        "TOCTOU: path disappeared between metadata checks"
                                    );
                                    return None;
                                }
                            };

                            if current_meta.is_symlink() {
                                // Was a regular file, replaced by a symlink.
                                // Treat it as a symlink now.
                                tracing::warn!(
                                    path = %entry_path.display(),
                                    "TOCTOU: regular file was replaced by symlink between metadata checks, treating as symlink"
                                );
                                let target = tokio::fs::read_link(&entry_path).await.ok()?;
                                let canonical = match tokio::fs::canonicalize(&entry_path).await {
                                    Ok(p) => p,
                                    Err(_) => return None,
                                };
                                if !canonical.starts_with(&share_canonical) {
                                    return None;
                                }
                                let uuid = handle_db
                                    .get_or_create_handle_non_canonical(&entry_path)
                                    .await
                                    .ok()?;
                                (uuid.as_bytes().to_vec(), Some(target.to_string_lossy().into_owned()), true)
                            } else {
                                // Still not a symlink — proceed as regular file/directory.
                                if !entry_canonical.starts_with(&share_canonical) {
                                    return None;
                                }
                                let uuid = handle_db
                                    .get_or_create_handle(&entry_canonical)
                                    .await
                                    .ok()?
                                    .as_bytes()
                                    .to_vec();
                                (uuid, None, false)
                            }
                        };

                        // Use the re-verified file type, not the initial one.
                        let proto_type = if final_is_symlink {
                            FileType::Symlink as i32
                        } else if file_type.is_dir() {
                            FileType::Directory as i32
                        } else {
                            FileType::Regular as i32
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

    // -----------------------------------------------------------------------
    // TOCTOU re-verification tests
    //
    // These tests verify that readdir_response correctly handles type
    // changes that could occur in a TOCTOU race. While we can't reproduce
    // the exact timing of a race, we verify the observable behavior:
    // if a symlink is replaced by a regular file (or vice versa) between
    // readdir calls, the reported file type is updated correctly.
    // -----------------------------------------------------------------------

    /// When a symlink in a directory is replaced by a regular file between
    /// two readdir calls, the second readdir must report FileType::Regular.
    #[tokio::test]
    #[cfg(unix)]
    async fn readdir_symlink_replaced_by_file_shows_regular_type() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Create a target file and a symlink pointing to it.
        std::fs::write(share.join("target.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("target.txt", share.join("entry")).unwrap();

        let handle_db = HandleDatabase::new();
        let dir_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = ReaddirRequest {
            directory_handle: dir_uuid.as_bytes().to_vec(),
            offset: 0,
            limit: 0,
        };
        let payload = req.encode_to_vec();

        // First readdir: should see a symlink.
        let resp = readdir_response(&payload, &share, &handle_db).await;
        let success = match resp.result {
            Some(readdir_response::Result::Entries(s)) => s,
            other => panic!("expected Entries, got: {:?}", other),
        };
        let entry = success
            .entries
            .iter()
            .find(|e| e.name == "entry")
            .expect("must list entry");
        assert_eq!(
            entry.file_type,
            FileType::Symlink as i32,
            "initial readdir must report FileType::Symlink"
        );

        // Replace the symlink with a regular file.
        std::fs::remove_file(share.join("entry")).unwrap();
        std::fs::write(share.join("entry"), b"replaced").unwrap();

        // Second readdir: must report FileType::Regular.
        let resp2 = readdir_response(&payload, &share, &handle_db).await;
        let success2 = match resp2.result {
            Some(readdir_response::Result::Entries(s)) => s,
            other => panic!("expected Entries, got: {:?}", other),
        };
        let entry2 = success2
            .entries
            .iter()
            .find(|e| e.name == "entry")
            .expect("must list entry after swap");
        assert_eq!(
            entry2.file_type,
            FileType::Regular as i32,
            "after symlink→file swap, readdir must report FileType::Regular"
        );
        assert!(
            entry2.symlink_target.is_empty(),
            "regular file must not have symlink_target"
        );
    }

    /// When a regular file in a directory is replaced by a symlink between
    /// two readdir calls, the second readdir must report FileType::Symlink
    /// with the correct symlink_target.
    #[tokio::test]
    #[cfg(unix)]
    async fn readdir_file_replaced_by_symlink_shows_symlink_type() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();

        // Start with a target file and a regular file named "entry".
        std::fs::write(share.join("target.txt"), b"hello").unwrap();
        std::fs::write(share.join("entry"), b"data").unwrap();

        let handle_db = HandleDatabase::new();
        let dir_uuid = handle_db.get_or_create_handle(&share).await.unwrap();

        let req = ReaddirRequest {
            directory_handle: dir_uuid.as_bytes().to_vec(),
            offset: 0,
            limit: 0,
        };
        let payload = req.encode_to_vec();

        // First readdir: should see a regular file.
        let resp = readdir_response(&payload, &share, &handle_db).await;
        let success = match resp.result {
            Some(readdir_response::Result::Entries(s)) => s,
            other => panic!("expected Entries, got: {:?}", other),
        };
        let entry = success
            .entries
            .iter()
            .find(|e| e.name == "entry")
            .expect("must list entry");
        assert_eq!(
            entry.file_type,
            FileType::Regular as i32,
            "initial readdir must report FileType::Regular"
        );

        // Replace the regular file with a symlink.
        std::fs::remove_file(share.join("entry")).unwrap();
        std::os::unix::fs::symlink("target.txt", share.join("entry")).unwrap();

        // Second readdir: must report FileType::Symlink with symlink_target.
        let resp2 = readdir_response(&payload, &share, &handle_db).await;
        let success2 = match resp2.result {
            Some(readdir_response::Result::Entries(s)) => s,
            other => panic!("expected Entries, got: {:?}", other),
        };
        let entry2 = success2
            .entries
            .iter()
            .find(|e| e.name == "entry")
            .expect("must list entry after swap");
        assert_eq!(
            entry2.file_type,
            FileType::Symlink as i32,
            "after file→symlink swap, readdir must report FileType::Symlink"
        );
        assert_eq!(
            entry2.symlink_target, "target.txt",
            "symlink_target must match the link target"
        );
    }
}
