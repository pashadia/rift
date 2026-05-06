use std::collections::HashMap;
use std::path::Path;

use prost::Message as _;
use tracing::instrument;

use rift_common::crypto::{Blake3Hash, Chunker, MerkleChild};
use rift_protocol::messages::{
    msg, MerkleChildProto, MerkleChildType, MerkleDrill, MerkleDrillResponse,
};
use rift_transport::RiftStream;

use uuid::Uuid;

use crate::handle::HandleDatabase;
use crate::handler::merkle_cache_trait::MerkleCache;
use crate::handler::resolve;

/// Convert internal `MerkleChild` enum values to proto `MerkleChildProto` messages.
fn children_to_proto(children: &[MerkleChild]) -> Vec<MerkleChildProto> {
    children
        .iter()
        .map(|c| match c {
            MerkleChild::Subtree(hash) => MerkleChildProto {
                child_type: MerkleChildType::MerkleChildSubtree as i32,
                hash: hash.as_bytes().to_vec(),
                length: 0,
                chunk_index: 0,
            },
            MerkleChild::Leaf {
                hash,
                length,
                chunk_index,
            } => MerkleChildProto {
                child_type: MerkleChildType::MerkleChildLeaf as i32,
                hash: hash.as_bytes().to_vec(),
                length: *length,
                chunk_index: *chunk_index,
            },
        })
        .collect()
}

/// Look up Merkle tree children, checking the in-memory cache first and
/// falling back to the database. Returns `None` if not found in either.
async fn resolve_children<M: MerkleCache>(
    cache: &HashMap<Blake3Hash, Vec<MerkleChild>>,
    canonical: &Path,
    query_hash: &Blake3Hash,
    database: &M,
) -> Option<Vec<MerkleChild>> {
    match cache.get(query_hash) {
        Some(c) => Some(c.clone()),
        None => match database.get_children(canonical, query_hash).await {
            Ok(Some(c)) => Some(c),
            Ok(None) => None,
            Err(_) => None,
        },
    }
}

/// Send an empty `MerkleDrillResponse` (zero parent hash, zero children) and finish the stream.
async fn send_empty_drill_response<S: RiftStream>(stream: &mut S) -> anyhow::Result<()> {
    let response = MerkleDrillResponse {
        parent_hash: vec![],
        children: vec![],
    };
    stream
        .send_frame(msg::MERKLE_DRILL_RESPONSE, &response.encode_to_vec())
        .await?;
    stream.finish_send().await?;
    Ok(())
}

/// Build the Merkle tree for a file and cache it in the database.
///
/// Uses the streaming utility `compute_file_merkle_tree` for a true
/// single-pass hash of all chunks, then persists the result to `db`.
/// Peak memory is `O(max_chunk_size)`.
async fn build_and_cache_tree<M: MerkleCache>(
    canonical: &Path,
    chunker: Chunker,
    db: &M,
) -> Option<(Blake3Hash, HashMap<Blake3Hash, Vec<MerkleChild>>)> {
    let (root, cache, leaf_infos) =
        crate::handler::merkle_cache::compute_file_merkle_tree(canonical, &chunker).await?;
    crate::handler::merkle_cache::cache_computed_tree(
        canonical,
        db,
        &root,
        cache.clone(),
        leaf_infos,
    )
    .await;
    Some((root, cache))
}

/// Handle a `MerkleDrill` request: look up children at a given hash in the file's
/// Merkle tree.
///
/// Algorithm (cache-first):
/// 1. Parse request and validate handle
/// 2. Resolve handle to canonical path
/// 3. Determine query hash (root hash from `get_merkle` cache, or from request)
/// 4. **Try database first**: if `get_children` returns cached data, return immediately
/// 5. If cache miss: read file, build tree, cache it, then look up children
/// 6. Convert to proto and send response
#[instrument(skip_all, fields(share = %share.display()), level = "debug")]
pub async fn merkle_drill_response<S: RiftStream, M: MerkleCache>(
    stream: &mut S,
    payload: &[u8],
    share: &Path,
    db: &M,
    handle_db: &HandleDatabase,
    chunker: Chunker,
) -> anyhow::Result<()> {
    let Ok(req) = MerkleDrill::decode(payload) else {
        return send_empty_drill_response(stream).await;
    };

    let Ok(handle) = Uuid::from_slice(&req.handle) else {
        return send_empty_drill_response(stream).await;
    };

    let canonical = match resolve(share, &handle, handle_db).await {
        Ok(r) => r.canonical,
        Err(_) => return send_empty_drill_response(stream).await,
    };

    // Step 1: Determine the query hash.
    // For root drill (empty hash): try DB cache for root hash first.
    // For subtree drill: parse the hash from the request.
    let query_hash = if req.hash.is_empty() {
        // Root drill — try to get root hash from DB cache
        match db.get_merkle(&canonical).await {
            Ok(Some(entry)) => entry.root,
            _ => {
                // Cache miss — must build the tree
                let Some((root, cache)) = build_and_cache_tree(&canonical, chunker, db).await
                else {
                    return send_empty_drill_response(stream).await;
                };

                // Look up root's children in the in-memory cache
                let Some(children) = resolve_children(&cache, &canonical, &root, db).await else {
                    return send_empty_drill_response(stream).await;
                };

                let response = MerkleDrillResponse {
                    parent_hash: root.as_bytes().to_vec(),
                    children: children_to_proto(&children),
                };
                stream
                    .send_frame(msg::MERKLE_DRILL_RESPONSE, &response.encode_to_vec())
                    .await?;
                stream.finish_send().await?;
                return Ok(());
            }
        }
    } else {
        match Blake3Hash::from_slice(&req.hash) {
            Ok(h) => h,
            Err(_) => return send_empty_drill_response(stream).await,
        }
    };

    // Step 2: Try DB first for the requested subtree hash
    if let Ok(Some(children)) = db.get_children(&canonical, &query_hash).await {
        let response = MerkleDrillResponse {
            parent_hash: query_hash.as_bytes().to_vec(),
            children: children_to_proto(&children),
        };
        stream
            .send_frame(msg::MERKLE_DRILL_RESPONSE, &response.encode_to_vec())
            .await?;
        stream.finish_send().await?;
        return Ok(());
    }

    // Step 3: Cache miss — read file and build tree
    let Some((root, cache)) = build_and_cache_tree(&canonical, chunker, db).await else {
        return send_empty_drill_response(stream).await;
    };

    // Step 4: Look up children at the query hash
    let Some(children) = resolve_children(&cache, &canonical, &query_hash, db).await else {
        return send_empty_drill_response(stream).await;
    };

    let parent_hash = if req.hash.is_empty() {
        root.as_bytes().to_vec()
    } else {
        query_hash.as_bytes().to_vec()
    };

    let response = MerkleDrillResponse {
        parent_hash,
        children: children_to_proto(&children),
    };
    stream
        .send_frame(msg::MERKLE_DRILL_RESPONSE, &response.encode_to_vec())
        .await?;
    stream.finish_send().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    use rift_common::crypto::{Chunker, MerkleTree};
    use rift_protocol::messages::MerkleDrill;

    use crate::handle::HandleDatabase;
    use crate::metadata::db::Database;

    use rift_protocol::messages::msg;
    use rift_transport::connection::InMemoryConnection;
    use rift_transport::RiftConnection;

    /// Direct test: `build_and_cache_tree` produces correct root and cache
    /// matching batch method, without materializing all chunk data.
    #[tokio::test]
    async fn build_and_cache_tree_produces_correct_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("drill_tree.bin");
        let content: Vec<u8> = (0u8..=255).cycle().take(300_000).collect();
        std::fs::write(&file, &content).unwrap();

        let chunker = Chunker::default();

        // Compute expected using batch method
        let chunk_boundaries = chunker.chunk(&content);
        let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
            .iter()
            .map(|(offset, length)| Blake3Hash::new(&content[*offset..*offset + *length]))
            .collect();
        let merkle = MerkleTree::default();
        let (expected_root, expected_cache, _) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

        // Call streaming implementation
        let db = Database::open_in_memory().await.unwrap();
        let result = build_and_cache_tree(&file, chunker, &db).await;
        assert!(result.is_some(), "must return Some for a valid file");
        let (root, cache) = result.unwrap();

        assert_eq!(root, expected_root, "root must match batch method");
        assert_eq!(
            cache.len(),
            expected_cache.len(),
            "cache entry count must match"
        );
        for (cache_hash, cache_children) in &cache {
            let expected_children = expected_cache
                .get(cache_hash)
                .expect("cache key must be in expected cache");
            assert_eq!(
                cache_children, expected_children,
                "cache children mismatch for hash"
            );
        }

        // Verify cache was persisted to DB
        let cached = db.get_children(&file, &root).await.unwrap();
        assert!(
            cached.is_some(),
            "cache must be populated in DB after build"
        );
    }

    /// SECURITY: Verifies that the `MerkleDrill` handler rejects handles pointing
    /// outside the share, testing the `resolve()` path traversal guard.
    #[tokio::test]
    async fn drill_rejects_path_traversal() {
        let share = tempfile::tempdir().unwrap();
        let share_canonical = std::fs::canonicalize(share.path()).unwrap();

        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, b"secret data").unwrap();

        let handle_db = HandleDatabase::new();
        let outside_handle = handle_db.get_or_create_handle(&outside_file).await.unwrap();

        let stored = handle_db.get_path(&outside_handle).unwrap();
        let stored_canonical = std::fs::canonicalize(&stored).unwrap();
        assert!(
            !stored_canonical.starts_with(&share_canonical),
            "test setup: handle must point outside share root"
        );

        let (client_conn, server_conn) = InMemoryConnection::pair();
        let mut client_stream = client_conn.open_stream().await.unwrap();
        let mut server_stream = server_conn.accept_stream().await.unwrap();

        let drill_req = MerkleDrill {
            handle: outside_handle.into_bytes().to_vec(),
            hash: vec![],
        };
        let payload = drill_req.encode_to_vec();

        let db = Database::open_in_memory().await.unwrap();

        merkle_drill_response(
            &mut server_stream,
            &payload,
            &share_canonical,
            &db,
            &handle_db,
            Chunker::default(),
        )
        .await
        .unwrap();

        let (type_id, resp_bytes) = client_stream.recv_frame().await.unwrap().unwrap();
        assert_eq!(type_id, msg::MERKLE_DRILL_RESPONSE);

        let response = MerkleDrillResponse::decode(&resp_bytes[..]).unwrap();

        assert!(
            response.parent_hash.is_empty(),
            "parent_hash must be empty for path traversal — got {:?}",
            response.parent_hash
        );
        assert!(
            response.children.is_empty(),
            "children must be empty for path traversal — got {} children",
            response.children.len()
        );
    }
}
