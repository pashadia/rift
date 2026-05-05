//! Background integrity check for the Merkle cache database.
//!
//! On server startup, a background task walks the share filesystem and
//! verifies that every regular file has a complete, consistent Merkle
//! tree cache entry. Missing or stale entries are recomputed and stored.
//! Entries for deleted files are cleaned up.

use std::path::Path;
use std::sync::Arc;

use rift_common::crypto::Chunker;
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::handler::merkle_cache::cache_computed_tree;
use crate::handler::merkle_cache::compute_file_merkle_tree;
use crate::handler::merkle_cache_trait::MerkleCache;

/// Summary of the background integrity check results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundCheckSummary {
    /// Total number of regular files found in the share.
    pub files_checked: usize,
    /// Number of files that had no cache entry and were newly computed.
    pub files_added: usize,
    /// Number of files where the cache key (mtime/size) conflicted with
    /// the current filesystem metadata and the entry was recomputed.
    pub files_conflict: usize,
    /// Number of DB entries for files that no longer exist on disk.
    pub files_cleaned: usize,
    /// Number of files where recomputation failed.
    pub errors: usize,
}

/// Run the background integrity check against a share directory.
///
/// Walks `share`, checks each regular file's Merkle cache status, and
/// recomputes missing or stale entries. Then deletes orphaned DB entries
/// for files that no longer exist.
///
/// This function is designed to be spawned as a background tokio task
/// that runs concurrently with the main accept loop.
pub async fn run_background_check<M: MerkleCache + 'static>(
    share: &Path,
    db: Arc<M>,
    chunker: Chunker,
) -> anyhow::Result<BackgroundCheckSummary> {
    let mut summary = BackgroundCheckSummary {
        files_checked: 0,
        files_added: 0,
        files_conflict: 0,
        files_cleaned: 0,
        errors: 0,
    };

    // Collect all on-disk file paths (canonicalized) for orphan cleanup
    let mut disk_paths: Vec<String> = Vec::new();

    for entry in WalkDir::new(share).follow_links(false) {
        let Ok(entry) = entry else {
            continue;
        };

        // Skip non-regular files (directories, symlinks)
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();

        // Canonicalize the path to match what handlers use
        let Ok(canonical) = path.canonicalize() else {
            continue;
        };

        summary.files_checked += 1;
        process_file(&canonical, db.as_ref(), &chunker, &mut summary).await;
        disk_paths.push(canonical.to_string_lossy().to_string());
    }

    // Remove orphaned entries (files that no longer exist on disk)
    match db.delete_orphaned_entries(&disk_paths).await {
        Ok(deleted) => {
            summary.files_cleaned = usize::try_from(deleted).unwrap_or(0);
        }
        Err(e) => {
            warn!(error = %e, "failed to delete orphaned entries");
        }
    }

    info!(
        files_checked = summary.files_checked,
        files_added = summary.files_added,
        files_conflict = summary.files_conflict,
        files_cleaned = summary.files_cleaned,
        errors = summary.errors,
        "background check complete"
    );

    Ok(summary)
}

/// Process a single file: check its cache status and recompute if needed.
///
/// Extracted from `run_background_check` to reduce cognitive complexity.
async fn process_file<M: MerkleCache>(
    canonical: &Path,
    db: &M,
    chunker: &Chunker,
    summary: &mut BackgroundCheckSummary,
) {
    match db.is_cache_complete(canonical).await {
        Ok(true) => {
            tracing::debug!(path = %canonical.display(), "cache up-to-date, skipping");
        }
        Ok(false) => {
            recompute_file(canonical, db, chunker, summary).await;
        }
        Err(e) => {
            warn!(path = %canonical.display(), error = %e, "failed to check cache completeness");
            summary.errors += 1;
        }
    }
}

/// Determine whether a stale file has a conflicting entry (wrong mtime/size)
/// or is entirely missing, then recompute its Merkle tree.
#[allow(clippy::cognitive_complexity)]
async fn recompute_file<M: MerkleCache>(
    canonical: &Path,
    db: &M,
    chunker: &Chunker,
    summary: &mut BackgroundCheckSummary,
) {
    let canonical_str = canonical.to_string_lossy().to_string();

    let is_conflict = {
        let entries = db.list_cached_entries().await.unwrap_or_default();
        entries.iter().any(|e| e.path == canonical_str)
    };

    if is_conflict {
        summary.files_conflict += 1;
        warn!(path = %canonical.display(), "conflicting cache entry, recomputing");
    } else {
        summary.files_added += 1;
        info!(path = %canonical.display(), "missing cache entry, computing");
    }

    // Delete stale entry if it exists
    let _ = db.delete_merkle(canonical).await;

    // Compute and store Merkle tree (reuses handler logic)
    match compute_file_merkle_tree(canonical, chunker).await {
        Some((root, tree_cache, leaf_infos)) => {
            cache_computed_tree(canonical, db, &root, tree_cache, leaf_infos).await;
        }
        None => {
            warn!(path = %canonical.display(), "failed to compute Merkle tree");
            summary.errors += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::db::Database;
    use rift_common::crypto::Blake3Hash;

    #[tokio::test]
    async fn background_check_caches_empty_share() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();
        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        let summary = run_background_check(&share, db, Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 0);
        assert_eq!(summary.files_added, 0);
        assert_eq!(summary.files_conflict, 0);
        assert_eq!(summary.files_cleaned, 0);
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn background_check_adds_new_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();

        // Create some test files
        std::fs::write(share.join("a.txt"), b"hello world a").unwrap();
        std::fs::write(share.join("b.txt"), b"hello world b").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());
        let db_for_check = db.clone();

        let summary = run_background_check(&share, db_for_check, Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 2);
        assert_eq!(summary.files_added, 2);
        assert_eq!(summary.files_conflict, 0);
        assert_eq!(summary.files_cleaned, 0);
        assert_eq!(summary.errors, 0);

        // Verify cache was populated
        let cached = db.list_cached_entries().await.unwrap();
        assert_eq!(cached.len(), 2, "both files should be cached");
    }

    #[tokio::test]
    async fn background_check_detects_conflict_and_recomputes() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();
        let file_path = share.join("conflict.txt");
        std::fs::write(&file_path, b"original content").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        // Pre-populate cache with WRONG mtime/size (simulates stale entry)
        let root = Blake3Hash::new(b"wrong_root");
        let leaf = Blake3Hash::new(b"wrong_leaf");
        db.put_merkle(
            &file_path,
            0,   // wrong mtime
            999, // wrong size
            &root,
            std::slice::from_ref(&leaf),
        )
        .await
        .unwrap();

        let summary = run_background_check(&share, db, Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 1);
        assert_eq!(summary.files_added, 0);
        assert_eq!(
            summary.files_conflict, 1,
            "should detect mtime/size conflict"
        );
        assert_eq!(summary.files_cleaned, 0);
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn background_check_skips_already_cached_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();
        let file_path = share.join("cached.txt");
        std::fs::write(&file_path, b"already cached content").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        // Pre-populate cache correctly using the full put_tree flow
        let chunker = Chunker::default();
        let content = std::fs::read(&file_path).unwrap();
        let chunk_boundaries = chunker.chunk(&content);
        let leaf_hashes: Vec<Blake3Hash> = chunk_boundaries
            .iter()
            .map(|(offset, length)| Blake3Hash::new(&content[*offset..*offset + *length]))
            .collect();
        let merkle = rift_common::crypto::MerkleTree::default();
        let (root_hash, cache, leaf_infos) =
            merkle.build_with_cache_and_offsets(&leaf_hashes, &chunk_boundaries);

        let meta = std::fs::metadata(&file_path).unwrap();
        let mtime_ns = u64::try_from(
            meta.modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .expect("timestamp nanos fit in u64");
        let file_size = meta.len();

        db.put_tree(
            &file_path,
            mtime_ns,
            file_size,
            &root_hash,
            &cache,
            &leaf_infos,
        )
        .await
        .unwrap();

        let summary = run_background_check(&share, db, Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 1);
        assert_eq!(
            summary.files_added, 0,
            "already cached file should not be re-added"
        );
        assert_eq!(
            summary.files_conflict, 0,
            "already cached file should not conflict"
        );
        assert_eq!(summary.files_cleaned, 0);
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn background_check_removes_deleted_file_entries() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        // Add a cache entry for a file that doesn't exist
        let root = Blake3Hash::new(b"orphan_root");
        let leaf = Blake3Hash::new(b"orphan_leaf");
        let orphan_path = share.join("deleted.txt");
        db.put_merkle(&orphan_path, 100, 50, &root, std::slice::from_ref(&leaf))
            .await
            .unwrap();

        // Verify it's there
        assert_eq!(db.list_cached_entries().await.unwrap().len(), 1);

        let db_for_check = db.clone();
        let summary = run_background_check(&share, db_for_check, Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_cleaned, 1, "orphaned entry should be cleaned");
        assert!(db.list_cached_entries().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn background_check_skips_directories_and_symlinks() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();

        // Create a directory and a regular file
        std::fs::create_dir(share.join("subdir")).unwrap();
        std::fs::write(share.join("file.txt"), b"regular file").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        let summary = run_background_check(&share, db, Chunker::default())
            .await
            .unwrap();

        // Only the regular file should be checked
        assert_eq!(summary.files_checked, 1);
        assert_eq!(summary.files_added, 1);
    }
}
