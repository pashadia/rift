use std::path::Path;

use futures::future::BoxFuture;
use futures::FutureExt;
use prost::Message as _;
use tracing::instrument;

use rift_common::crypto::Chunker;
use rift_protocol::messages::{stat_result, ErrorCode, StatRequest, StatResponse, StatResult};

use uuid::Uuid;

use crate::handle::HandleDatabase;
use crate::handler::attrs::build_attrs;
use crate::handler::merkle_cache::get_or_compute_merkle_root;
use crate::handler::merkle_cache_trait::MerkleCache;
use crate::handler::{error_detail, io_err_kind_to_code, resolve};

/// Handle a `StatRequest`: stat each requested handle and return one
/// `StatResult` per handle (success or error).
///
/// Malformed payloads return an empty result list rather than panicking.
/// Each handle in the request produces exactly one result in the response,
/// preserving a 1:1 invariant. Invalid handles (wrong byte count, etc.)
/// produce an `ErrorNotFound` result rather than being silently dropped.
#[instrument(skip(share, db, handle_db), fields(share = %share.display()), level = "debug")]
pub async fn stat_response<M: MerkleCache>(
    payload: &[u8],
    share: &Path,
    db: &M,
    handle_db: &HandleDatabase,
    chunker: Chunker,
) -> StatResponse {
    let req = match StatRequest::decode(payload) {
        Ok(r) => r,
        Err(_) => return StatResponse { results: vec![] },
    };

    let futures: Vec<_> = req
        .handles
        .into_iter()
        .map(|handle_bytes| match Uuid::from_slice(&handle_bytes) {
            Ok(uuid) => async_stat(share, handle_bytes, uuid, handle_db, db, chunker).boxed(),
            Err(_) => async { stat_error(ErrorCode::ErrorNotFound) }.boxed(),
        })
        .collect();

    let results = futures::future::join_all(futures).await;

    StatResponse { results }
}

fn stat_error(code: ErrorCode) -> StatResult {
    StatResult {
        handle: Vec::new(),
        result: Some(stat_result::Result::Error(error_detail(code))),
    }
}

fn async_stat<'a, M: MerkleCache>(
    share: &'a Path,
    handle_bytes: Vec<u8>,
    uuid: Uuid,
    handle_db: &'a HandleDatabase,
    db: &'a M,
    chunker: Chunker,
) -> BoxFuture<'a, StatResult> {
    Box::pin(async move {
        let canonical = match resolve(share, &uuid, handle_db).await {
            Ok(r) => r.canonical,
            Err(e) => {
                let code = e
                    .root_cause()
                    .downcast_ref::<std::io::Error>()
                    .map(|io| io_err_kind_to_code(io.kind()))
                    .unwrap_or(ErrorCode::ErrorNotFound);
                return stat_error(code);
            }
        };

        let meta = match tokio::fs::metadata(&canonical).await {
            Ok(m) => m,
            Err(e) => {
                return stat_error(io_err_kind_to_code(e.kind()));
            }
        };

        let root_hash = get_or_compute_merkle_root(&canonical, &meta, db, chunker).await;
        StatResult {
            handle: handle_bytes,
            result: Some(stat_result::Result::Attrs(build_attrs(&meta, root_hash))),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use tempfile::TempDir;

    use crate::handle::HandleDatabase;
    use crate::handler::merkle_cache_trait::NoopCache;
    use rift_common::crypto::Chunker;
    use rift_protocol::messages::{FileType, StatRequest};

    #[tokio::test]
    async fn stat_response_returns_attrs_for_valid_handle() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let file = share.join("stat_me.txt");
        let content = b"stat content";
        std::fs::write(&file, content).unwrap();

        let handle_db = HandleDatabase::new();
        let uuid = handle_db.get_or_create_handle(&file).await.unwrap();

        let req = StatRequest {
            handles: vec![uuid.as_bytes().to_vec()],
        };
        let payload = req.encode_to_vec();

        let resp =
            stat_response(&payload, &share, &NoopCache, &handle_db, Chunker::default()).await;

        assert_eq!(resp.results.len(), 1);
        match &resp.results[0].result {
            Some(stat_result::Result::Attrs(attrs)) => {
                assert_eq!(attrs.size, content.len() as u64);
                assert_eq!(attrs.file_type, FileType::Regular as i32);
            }
            other => panic!("expected Attrs, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn stat_response_malformed_payload_returns_empty_results() {
        let tmp = TempDir::new().unwrap();
        let share = tmp.path().to_path_buf();
        let handle_db = HandleDatabase::new();

        let garbage = vec![0xFF, 0xFE, 0x00, 0xAB, 0xCD];
        let resp =
            stat_response(&garbage, &share, &NoopCache, &handle_db, Chunker::default()).await;

        assert_eq!(
            resp.results.len(),
            0,
            "malformed payload must yield empty results"
        );
    }
}
