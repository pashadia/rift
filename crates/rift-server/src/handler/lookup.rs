use std::path::Path;

use prost::Message as _;
use tracing::instrument;

use rift_common::crypto::Chunker;
use rift_protocol::messages::{
    lookup_response, ErrorCode, LookupRequest, LookupResponse, LookupResult,
};

use uuid::Uuid;

use crate::handle::HandleDatabase;
use crate::handler::attrs::build_attrs;
use crate::handler::merkle_cache::get_or_compute_merkle_root;
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
        Ok(p) => p,
        Err(_) => return lookup_error(ErrorCode::ErrorNotFound),
    };

    let share_canonical = match tokio::fs::canonicalize(share).await {
        Ok(p) => p,
        Err(e) => return lookup_error(io_err_kind_to_code(e.kind())),
    };

    let child_path = parent_canonical.join(&req.name);

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
            attrs: Some(build_attrs(&meta, root_hash)),
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
    use rift_protocol::messages::LookupRequest;

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
}
