use std::path::Path;

use rift_common::crypto::{Blake3Hash, Chunker, MerkleTree};
use tracing::instrument;

use crate::metadata::db::Database;

pub(crate) fn root_hash_for_type(is_dir: bool) -> Blake3Hash {
    if is_dir {
        Blake3Hash::new(b"<directory>")
    } else {
        Blake3Hash::new(b"<symlink>")
    }
}

/// Get or compute the Merkle root hash for a file.
///
/// Always returns a 32-byte Blake3Hash:
/// - For regular files: Merkle root computed from content (cached if possible)
/// - For non-files (directories, etc.): uses a constant sentinel hash
#[instrument(skip_all, fields(path = %path.display()), level = "debug")]
pub(crate) async fn get_or_compute_merkle_root(
    path: &Path,
    meta: &std::fs::Metadata,
    db: Option<&Database>,
    chunker: Chunker,
) -> Blake3Hash {
    if !meta.is_file() {
        return root_hash_for_type(true);
    }

    if let Some(database) = db {
        match database.get_merkle(path).await {
            Ok(Some(entry)) => return entry.root,
            Ok(None) => {}
            Err(_) => {}
        }
    }

    let content = match tokio::fs::read(path).await {
        Ok(c) => c,
        Err(_) => return root_hash_for_type(true),
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

    if let Some(database) = db {
        if let Ok(file_meta) = tokio::fs::metadata(path).await {
            let mtime_ns = match file_meta.modified() {
                Ok(t) => t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0),
                Err(_) => 0,
            };
            let file_size = file_meta.len();
            if let Err(e) = database
                .put_merkle(path, mtime_ns, file_size, &root, &leaf_hashes)
                .await
            {
                tracing::warn!(path = %path.display(), error = %e, "failed to cache merkle root");
            }
        }
    }

    root
}
