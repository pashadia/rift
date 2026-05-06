use std::collections::HashMap;
use std::path::Path;

use futures::StreamExt;
use rift_common::crypto::{Blake3Hash, Chunker, LeafInfo, MerkleChild, MerkleTree};
use rift_protocol::messages::FileType;
use tracing::instrument;

use super::merkle_cache_trait::MerkleCache;

pub(crate) fn sentinel_hash_for_non_file(file_type: FileType) -> Blake3Hash {
    match file_type {
        FileType::Directory => Blake3Hash::new(b"<directory>"),
        FileType::Symlink => Blake3Hash::new(b"<symlink>"),
        FileType::Regular => unreachable!(
            "regular files require content-based Merkle root; use get_or_compute_merkle_root()"
        ),
        _ => unreachable!("unexpected file type: {:?}", file_type),
    }
}

/// Get or compute the Merkle root hash for a file.
///
/// Always returns a 32-byte `Blake3Hash`:
/// - For regular files: Merkle root computed from content (cached if possible)
/// - For non-files (directories, etc.): uses a constant sentinel hash
#[instrument(skip_all, fields(path = %path.display()), level = "debug")]
pub(crate) async fn get_or_compute_merkle_root<M: MerkleCache>(
    path: &Path,
    meta: &std::fs::Metadata,
    cache: &M,
    chunker: Chunker,
) -> Blake3Hash {
    // Handle non-files (directories, symlinks, etc.)
    if !meta.is_file() {
        let file_type = classify_file_type(meta);
        return sentinel_hash_for_non_file(file_type);
    }

    // Try cache first (errors are non-fatal, just continue)
    if let Ok(Some(entry)) = cache.get_merkle(path).await {
        return entry.root;
    }

    // Compute Merkle root from file content
    match compute_file_merkle_tree(path, &chunker).await {
        Some((root, tree_cache, leaf_infos)) => {
            // Cache the result (errors are logged but don't fail the operation)
            cache_computed_tree(path, cache, &root, tree_cache, leaf_infos).await;
            root
        }
        None => {
            tracing::error!(path = %path.display(), "failed to compute merkle root");
            Blake3Hash::new(&[])
        }
    }
}

/// Classify metadata into a `FileType`.
fn classify_file_type(meta: &std::fs::Metadata) -> FileType {
    if meta.is_dir() {
        FileType::Directory
    } else if meta.is_symlink() {
        FileType::Symlink
    } else {
        tracing::error!("unexpected file type: {:#?}", meta.file_type());
        unreachable!("unexpected file type: expected directory or symlink")
    }
}

/// Compute the Merkle tree for a regular file by chunking and hashing.
/// Returns the root hash, the tree cache, and leaf infos for caching.
///
/// This is a shared utility used by both request handlers and the
/// background integrity check.
pub(crate) async fn compute_file_merkle_tree(
    path: &Path,
    chunker: &Chunker,
) -> Option<(
    Blake3Hash,
    HashMap<Blake3Hash, Vec<MerkleChild>>,
    Vec<LeafInfo>,
)> {
    // Open file and stream chunks one at a time.
    // Each chunk is hashed immediately and the data is dropped,
    // keeping peak memory at O(max_chunk_size).
    let file = tokio::fs::File::open(path).await.ok()?;
    let reader = tokio::io::BufReader::with_capacity(512 * 1024, file);

    let stream = chunker.chunk_stream(reader);
    let mut leaf_hashes = Vec::new();
    let mut chunk_boundaries = Vec::new();

    futures::pin_mut!(stream);
    while let Some(chunk_result) = stream.next().await {
        let (offset, length, data) = chunk_result.ok()?;
        // Hash immediately — BLAKE3 is fast enough for async task
        let hash = Blake3Hash::new(&data);
        leaf_hashes.push(hash);
        chunk_boundaries.push((offset, length));
        // data is dropped here
    }

    // CPU-bound: build Merkle tree with cache from collected hashes
    tokio::task::spawn_blocking(move || {
        let merkle = MerkleTree::default();
        let (root, cache, leaf_infos) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);
        Some((root, cache, leaf_infos))
    })
    .await
    .ok()?
}

/// Cache a computed Merkle tree with current file metadata.
///
/// This is a shared utility used by both request handlers and the
/// background integrity check.
///
/// `mtime_ns` is `Option<u64>`:
/// - `None` means unknown mtime (will store NULL in database)
/// - `Some(0)` means actual Unix epoch timestamp
pub(crate) async fn cache_computed_tree<M: MerkleCache>(
    path: &Path,
    cache: &M,
    root: &Blake3Hash,
    tree_cache: HashMap<Blake3Hash, Vec<MerkleChild>>,
    leaf_infos: Vec<LeafInfo>,
) {
    let Ok(file_meta) = tokio::fs::metadata(path).await else {
        return;
    };

    let mtime_ns = file_meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_nanos()).ok());

    let file_size = file_meta.len();

    if let Err(e) = cache
        .put_tree(path, mtime_ns, file_size, root, &tree_cache, &leaf_infos)
        .await
    {
        tracing::warn!(path = %path.display(), error = %e, "failed to cache merkle tree");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::db::Database;

    /// Direct test of `compute_file_merkle_tree`: verifies streaming impl
    /// produces correct root, cache, and `leaf_infos` matching batch method.
    #[tokio::test]
    async fn compute_file_merkle_tree_returns_correct_root_and_leaf_infos() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("direct_test.bin");
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
        let (expected_root, expected_cache, expected_leaf_infos) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

        // Call streaming implementation
        let result = compute_file_merkle_tree(&file, &chunker).await;
        assert!(result.is_some(), "must return Some for a valid file");
        let (root, cache, leaf_infos) = result.unwrap();

        assert_eq!(root, expected_root, "root must match batch method");
        assert_eq!(
            leaf_infos.len(),
            expected_leaf_infos.len(),
            "leaf_infos count must match"
        );

        // Verify each leaf info
        for (i, info) in leaf_infos.iter().enumerate() {
            assert_eq!(
                info.hash, expected_leaf_infos[i].hash,
                "leaf {i} hash mismatch"
            );
            assert_eq!(
                info.offset, expected_leaf_infos[i].offset,
                "leaf {i} offset mismatch"
            );
            assert_eq!(
                info.length, expected_leaf_infos[i].length,
                "leaf {i} length mismatch"
            );
            assert_eq!(
                info.chunk_index, expected_leaf_infos[i].chunk_index,
                "leaf {i} chunk_index mismatch"
            );
        }

        // Verify cache has same keys as expected
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
    }

    #[tokio::test]
    async fn get_or_compute_merkle_root_uses_streaming() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("merkle_stream.txt");
        let content: Vec<u8> = (0u8..=255).cycle().take(300_000).collect();
        std::fs::write(&file, &content).unwrap();

        let meta = std::fs::metadata(&file).unwrap();
        let chunker = Chunker::default();

        // Compute expected root using batch method
        let chunk_boundaries = chunker.chunk(&content);
        let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
            .iter()
            .map(|(offset, length)| Blake3Hash::new(&content[*offset..*offset + *length]))
            .collect();
        let merkle = MerkleTree::default();
        let expected_root = merkle.build(&leaf_hashes);

        let db = Database::open_in_memory().await.unwrap();

        let root = get_or_compute_merkle_root(&file, &meta, &db, chunker).await;

        assert_eq!(root, expected_root, "streaming root must match batch root");

        // Verify cache was populated
        let cached = db.get_merkle(&file).await.unwrap();
        assert!(
            cached.is_some(),
            "cache must be populated after computation"
        );
        // Verify tree cache was populated by checking leaf info was cached
        let leaf_info = db.get_all_leaf_info(&file).await.unwrap();
        assert!(
            leaf_info.is_some() && !leaf_info.as_ref().unwrap().is_empty(),
            "leaf info must be cached after computation"
        );

        // Verify the first leaf matches expected hash
        let expected_first_chunk_hash = Blake3Hash::new(&content[..chunk_boundaries[0].1]);
        let first_leaf = &leaf_info.unwrap()[0];
        assert_eq!(
            first_leaf.hash, expected_first_chunk_hash,
            "first leaf hash must match expected chunk hash"
        );
    }
}
