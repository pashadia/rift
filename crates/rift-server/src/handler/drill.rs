use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::Path;
use std::sync::Arc;

use prost::Message as _;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Semaphore;
use tracing::instrument;

use rift_common::crypto::{Blake3Hash, Chunker, MerkleChild, MerkleTree};
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

/// Read one chunk from the file at `offset` with the given `length` and spawn a
/// blocking task to compute its BLAKE3 hash.
///
/// The semaphore permit is held until the hash completes, bounding memory usage.
///
/// # Errors
///
/// Returns `None` and logs a warning if the semaphore is closed or a seek/read fails.
async fn read_and_hash_one_chunk(
    file: &mut tokio::fs::File,
    sem: &Arc<Semaphore>,
    chunk_index: usize,
    offset: usize,
    length: usize,
) -> Option<tokio::task::JoinHandle<(usize, Blake3Hash, Vec<u8>)>> {
    let permit = sem
        .clone()
        .acquire_owned()
        .await
        .inspect_err(
            |e| tracing::warn!(%chunk_index, error = %e, "semaphore closed during chunk hashing"),
        )
        .ok()?;

    let mut buf = vec![0u8; length];
    file.seek(SeekFrom::Start(offset as u64))
        .await
        .inspect_err(|e| tracing::warn!(%chunk_index, %offset, error = %e, "seek failed during chunk hashing"))
        .ok()?;
    file.read_exact(&mut buf)
        .await
        .inspect_err(|e| tracing::warn!(%chunk_index, %length, error = %e, "read failed during chunk hashing"))
        .ok()?;

    Some(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let hash = Blake3Hash::new(&buf);
        (chunk_index, hash, buf)
    }))
}

/// Read chunks from a file at the given boundary offsets and compute their BLAKE3 hashes.
///
/// Uses bounded parallelism (`available_parallelism` or 4) to hash chunks concurrently
/// without exhausting memory: a semaphore limits the number of in-flight `spawn_blocking`
/// hash operations. Chunks are read sequentially (single file descriptor) but hashed in
/// parallel on the blocking thread-pool.
///
/// # Errors
///
/// Returns `None` and logs a warning if:
/// - The file cannot be opened.
/// - A seek or read operation fails.
/// - The semaphore is closed.
/// - A `spawn_blocking` task panics.
async fn read_and_hash_chunks(
    canonical: &Path,
    chunk_boundaries: &[(usize, usize)],
) -> Option<Vec<(usize, Blake3Hash, Vec<u8>)>> {
    let concurrency = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let sem = Arc::new(Semaphore::new(concurrency));

    let mut file = tokio::fs::File::open(canonical)
        .await
        .inspect_err(|e| tracing::warn!(path = %canonical.display(), error = %e, "failed to open file for chunk hashing"))
        .ok()?;

    let mut handles = Vec::with_capacity(chunk_boundaries.len());
    for (chunk_index, (offset, length)) in chunk_boundaries.iter().enumerate() {
        let handle =
            read_and_hash_one_chunk(&mut file, &sem, chunk_index, *offset, *length).await?;
        handles.push(handle);
    }

    // Collect results preserving chunk-index order.
    let mut indexed_results: Vec<_> = Vec::with_capacity(handles.len());
    for h in handles {
        let result = h
            .await
            .inspect_err(
                |e| tracing::warn!(error = %e, "spawn_blocking task panicked during chunk hashing"),
            )
            .ok()?;
        indexed_results.push(result);
    }
    indexed_results.sort_by_key(|(idx, _, _)| *idx);

    Some(indexed_results)
}

/// Build a Merkle tree from leaf hashes and persist it to the database cache.
///
/// The tree is constructed on the blocking thread-pool via `spawn_blocking` to avoid
/// stalling the async runtime during CPU-intensive tree assembly. After construction
/// the tree is written to `db` on a best-effort basis — a cache write failure is logged
/// but does not cause the operation to fail.
///
/// # Errors
///
/// Returns `None` and logs a warning if `spawn_blocking` panics.
async fn build_tree_and_cache<M: MerkleCache>(
    canonical: &Path,
    leaf_hashes: Vec<Blake3Hash>,
    chunk_boundaries: &[(usize, usize)],
    db: &M,
) -> Option<(Blake3Hash, HashMap<Blake3Hash, Vec<MerkleChild>>)> {
    let chunk_boundaries = chunk_boundaries.to_vec();
    let result = tokio::task::spawn_blocking(move || {
        let merkle = MerkleTree::default();
        merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries)
    })
    .await
    .inspect_err(|e| tracing::warn!(path = %canonical.display(), error = %e, "spawn_blocking panicked in Merkle tree build"))
    .ok()?;

    let (root, cache, leaf_infos) = result;

    // Store tree in database (best-effort) — failure is non-fatal for the caller.
    if let Ok(meta) = tokio::fs::metadata(canonical).await {
        let mtime_ns = meta
            .modified()
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .map(|d| u64::try_from(d.as_nanos()).expect("timestamp nanos fit in u64"))
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let file_size = meta.len();
        if let Err(e) = db
            .put_tree(canonical, mtime_ns, file_size, &root, &cache, &leaf_infos)
            .await
        {
            tracing::warn!(path = %canonical.display(), error = %e, "failed to cache merkle tree");
        }
    }

    Some((root, cache))
}

/// Build the Merkle tree for a file and cache it in the database.
///
/// Orchestrates the full pipeline:
/// 1. Open the file and compute chunk boundaries via `Chunker::chunk_stream`.
/// 2. Read each chunk and compute its BLAKE3 hash concurrently (`read_and_hash_chunks`).
/// 3. Assemble the Merkle tree and persist it (`build_tree_and_cache`).
///
/// # Errors
///
/// Returns `None` and logs a warning if:
/// - The file cannot be opened.
/// - Chunk reading or tree construction fails (delegated to sub-functions).
async fn build_and_cache_tree<M: MerkleCache>(
    canonical: &Path,
    chunker: Chunker,
    db: &M,
) -> Option<(Blake3Hash, HashMap<Blake3Hash, Vec<MerkleChild>>)> {
    let file = tokio::fs::File::open(canonical)
        .await
        .inspect_err(|e| tracing::warn!(path = %canonical.display(), error = %e, "failed to open file for tree build"))
        .ok()?;
    let reader = tokio::io::BufReader::with_capacity(512 * 1024, file);
    let chunk_boundaries = chunker.chunk_stream(reader).await;

    let indexed_results = read_and_hash_chunks(canonical, &chunk_boundaries).await?;
    let leaf_hashes: Vec<Blake3Hash> = indexed_results.iter().map(|(_, h, _)| h.clone()).collect();

    build_tree_and_cache(canonical, leaf_hashes, &chunk_boundaries, db).await
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

    use rift_common::crypto::Chunker;
    use rift_protocol::messages::MerkleDrill;

    use crate::handle::HandleDatabase;
    use crate::metadata::db::Database;

    use rift_protocol::messages::msg;
    use rift_transport::connection::InMemoryConnection;
    use rift_transport::RiftConnection;

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
