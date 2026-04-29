use std::io::SeekFrom;
use std::path::Path;

use rift_common::crypto::{Blake3Hash, Chunker, MerkleTree};
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

    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
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
    let reader = tokio::io::BufReader::with_capacity(512 * 1024, file);
    let chunk_boundaries = chunker.chunk_stream(reader).await;

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
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

    // Read all chunk data with async I/O
    let mut chunk_data: Vec<Vec<u8>> = Vec::with_capacity(chunk_boundaries.len());
    for (offset, length) in &chunk_boundaries {
        let mut buf = vec![0u8; *length];
        if file.seek(SeekFrom::Start(*offset as u64)).await.is_err() {
            let file_type = if meta.is_dir() {
                FileType::Directory
            } else if meta.is_symlink() {
                FileType::Symlink
            } else {
                unreachable!("unexpected file type: expected directory or symlink")
            };
            return sentinel_hash_for_non_file(file_type);
        }
        if file.read_exact(&mut buf).await.is_err() {
            let file_type = if meta.is_dir() {
                FileType::Directory
            } else if meta.is_symlink() {
                FileType::Symlink
            } else {
                unreachable!("unexpected file type: expected directory or symlink")
            };
            return sentinel_hash_for_non_file(file_type);
        }
        chunk_data.push(buf);
    }

    // CPU-bound work: hash chunks and build Merkle tree in spawn_blocking
    let root = tokio::task::spawn_blocking(move || {
        let leaf_hashes: Vec<Blake3Hash> = chunk_data
            .iter()
            .map(|data| Blake3Hash::new(data))
            .collect();

        let merkle = MerkleTree::default();
        merkle.build(&leaf_hashes)
    })
    .await
    .unwrap_or_else(|_| Blake3Hash::new(&[]));

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
            .put_merkle(path, mtime_ns, file_size, &root, &[])
            .await
        {
            tracing::warn!(path = %path.display(), error = %e, "failed to cache merkle root");
        }
    }

    root
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
        assert_eq!(cached.unwrap().root, expected_root);
    }
}
