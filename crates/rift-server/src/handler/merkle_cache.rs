use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::Path;

use rift_common::crypto::{Blake3Hash, Chunker, LeafInfo, MerkleChild, MerkleTree};
use rift_protocol::messages::FileType;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
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
/// Always returns a 32-byte Blake3Hash:
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

/// Classify metadata into a FileType.
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
async fn compute_file_merkle_tree(
    path: &Path,
    chunker: &Chunker,
) -> Option<(
    Blake3Hash,
    HashMap<Blake3Hash, Vec<MerkleChild>>,
    Vec<LeafInfo>,
)> {
    // Open file and compute chunk boundaries using streaming
    let file = tokio::fs::File::open(path).await.ok()?;
    let reader = tokio::io::BufReader::with_capacity(512 * 1024, file);
    let chunk_boundaries = chunker.chunk_stream(reader).await;

    // Re-open for reading chunk data
    let mut file = tokio::fs::File::open(path).await.ok()?;

    // Read all chunk data
    let mut chunk_data: Vec<Vec<u8>> = Vec::with_capacity(chunk_boundaries.len());
    for (offset, length) in &chunk_boundaries {
        let mut buf = vec![0u8; *length];
        file.seek(SeekFrom::Start(*offset as u64)).await.ok()?;
        file.read_exact(&mut buf).await.ok()?;
        chunk_data.push(buf);
    }

    // CPU-bound: hash chunks and build Merkle tree with cache
    tokio::task::spawn_blocking(move || {
        let leaf_hashes: Vec<Blake3Hash> = chunk_data
            .iter()
            .map(|data| Blake3Hash::new(data))
            .collect();
        let merkle = MerkleTree::default();
        let (root, cache, leaf_infos) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);
        Some((root, cache, leaf_infos))
    })
    .await
    .ok()?
}

/// Cache a computed Merkle tree with current file metadata.
async fn cache_computed_tree<M: MerkleCache>(
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
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

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

    #[tokio::test]
    async fn get_or_compute_merkle_root_uses_streaming() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("merkle_stream.txt");
        let content: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
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
