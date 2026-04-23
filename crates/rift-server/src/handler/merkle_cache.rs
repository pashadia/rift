use std::path::Path;

use rift_common::crypto::{Blake3Hash, Chunker, MerkleTree};
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
    if !meta.is_file() {
        let file_type = if meta.is_dir() {
            FileType::Directory
        } else if meta.is_symlink() {
            FileType::Symlink
        } else {
            unreachable!("unexpected file type: expected directory or symlink")
        };
        return sentinel_hash_for_non_file(file_type);
    }

    match cache.get_merkle(path).await {
        Ok(Some(entry)) => return entry.root,
        Ok(None) => {}
        Err(_) => {}
    }

    let content = match tokio::fs::read(path).await {
        Ok(c) => c,
        Err(_) => {
            let file_type = if meta.is_dir() {
                FileType::Directory
            } else if meta.is_symlink() {
                FileType::Symlink
            } else {
                unreachable!("unexpected file type: expected directory or symlink")
            };
            return sentinel_hash_for_non_file(file_type);
        }
    };

    let chunk_boundaries = chunker.chunk(&content);

    let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
        .iter()
        .map(|(offset, length)| {
            let chunk_data = &content[*offset..*offset + length];
            Blake3Hash::new(chunk_data)
        })
        .collect();

    let merkle = MerkleTree::default();
    let root = merkle.build(&leaf_hashes);

    if let Ok(file_meta) = tokio::fs::metadata(path).await {
        let mtime_ns = match file_meta.modified() {
            Ok(t) => t
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
            Err(_) => 0,
        };
        let file_size = file_meta.len();
        if let Err(e) = cache
            .put_merkle(path, mtime_ns, file_size, &root, &leaf_hashes)
            .await
        {
            tracing::warn!(path = %path.display(), error = %e, "failed to cache merkle root");
        }
    }

    root
}
