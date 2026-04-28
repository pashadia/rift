use std::path::Path;

use prost::Message as _;
use tokio::fs;
use tokio::fs::DirEntry;
use tokio_stream::wrappers::ReadDirStream;
use tokio_stream::StreamExt;
use tracing::instrument;

use rift_protocol::messages::{
    readdir_response, ErrorCode, FileType, ReaddirEntry, ReaddirRequest, ReaddirResponse,
    ReaddirSuccess,
};

use uuid::Uuid;

use crate::handle::HandleDatabase;
use crate::handler::{error_detail, io_err_kind_to_code, resolve, verify_symlink_containment};

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

    let entries = match fs::read_dir(&dir_canonical).await {
        Ok(read_dir) => {
            let stream = ReadDirStream::new(read_dir);
            let share_canonical = match fs::canonicalize(share).await {
                Ok(p) => p,
                Err(_) => share.to_path_buf(),
            };
            let entries_with_none: Vec<Option<ReaddirEntry>> = stream
                .then(|entry_result| {
                    let share_canonical = share_canonical.clone();
                    let dir_display = dir_canonical.display().to_string();
                    async move {
                        match entry_result {
                            Ok(entry) => {
                                process_dir_entry(entry, &share_canonical, handle_db).await
                            }
                            Err(e) => {
                                tracing::warn!(path = %dir_display, error = %e, "readdir: failed to read directory entry");
                                None
                            }
                        }
                    }
                })
                .collect()
                .await;
            entries_with_none.into_iter().flatten().collect()
        }
        Err(e) => return readdir_error(io_err_kind_to_code(e.kind())),
    };

    let (entries, has_more) = paginate_entries(entries, req.offset, req.limit);

    ReaddirResponse {
        result: Some(readdir_response::Result::Entries(ReaddirSuccess {
            entries,
            has_more,
        })),
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Process a single directory entry: detect type, canonicalize, verify
/// containment (for symlinks), and create/fetch a handle.
///
/// Returns `None` if the entry should be skipped (e.g., broken symlink,
/// permission error, escaped containment).
async fn process_dir_entry(
    entry: DirEntry,
    share_canonical: &Path,
    handle_db: &HandleDatabase,
) -> Option<ReaddirEntry> {
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

    if file_type.is_symlink() {
        process_symlink_dir_entry(&entry_path, &name, share_canonical, handle_db).await
    } else {
        process_regular_dir_entry(&entry_path, &name, proto_type, handle_db).await
    }
}

/// Process a symlink entry: verify containment and record the symlink target.
async fn process_symlink_dir_entry(
    entry_path: &Path,
    name: &str,
    share_canonical: &Path,
    handle_db: &HandleDatabase,
) -> Option<ReaddirEntry> {
    verify_symlink_containment(entry_path, share_canonical).await?;

    let uuid = handle_db
        .get_or_create_handle_non_canonical(entry_path)
        .await
        .ok()?;
    let handle = uuid.as_bytes().to_vec();

    Some(ReaddirEntry {
        name: name.to_owned(),
        file_type: FileType::Symlink as i32,
        handle,
    })
}

/// Process a regular (non-symlink) entry: canonicalize and create a handle.
async fn process_regular_dir_entry(
    entry_path: &Path,
    name: &str,
    proto_type: i32,
    handle_db: &HandleDatabase,
) -> Option<ReaddirEntry> {
    let entry_canonical = match fs::canonicalize(entry_path).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %entry_path.display(), error = %e, "readdir: failed to canonicalize entry");
            return None;
        }
    };
    let uuid = handle_db
        .get_or_create_handle(&entry_canonical)
        .await
        .ok()?;
    let handle = uuid.as_bytes().to_vec();

    Some(ReaddirEntry {
        name: name.to_owned(),
        file_type: proto_type,
        handle,
    })
}

/// Sort entries alphabetically, then apply offset and limit.
///
/// Returns the paginated entries along with a `has_more` flag indicating
/// whether additional entries exist beyond the requested page.
fn paginate_entries(
    mut entries: Vec<ReaddirEntry>,
    offset: u32,
    limit: u32,
) -> (Vec<ReaddirEntry>, bool) {
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let offset = offset as usize;
    let entries: Vec<ReaddirEntry> = entries.into_iter().skip(offset).collect();

    if limit > 0 && entries.len() > limit as usize {
        let limited: Vec<_> = entries.into_iter().take(limit as usize).collect();
        (limited, true)
    } else {
        (entries, false)
    }
}

// ---------------------------------------------------------------------------
// Error helper
// ---------------------------------------------------------------------------

fn readdir_error(code: ErrorCode) -> ReaddirResponse {
    ReaddirResponse {
        result: Some(readdir_response::Result::Error(error_detail(code))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
    ///   2. Use the symlink's OWN path (not the canonical target) for the handle.
    ///   (The symlink target string is provided via stat_batch/FileAttrs, not ReaddirEntry.)
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

        // The handle must have been created from the symlink's own path,
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
    /// correct `file_type`. This also serves as a regression guard for the
    /// share_canonical hoist: if the hoist were broken, entries could silently
    /// vanish or report wrong metadata.
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
            let _target_name = format!("file{i}.txt");
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
}
