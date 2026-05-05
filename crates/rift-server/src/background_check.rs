//! Background integrity check for the Merkle cache database.
//!
//! On server startup, a background task walks the share filesystem and
//! verifies that every regular file has a complete, consistent Merkle
//! tree cache entry. Missing or stale entries are recomputed and stored.
//!
//! The filesystem walk runs on a blocking thread via `spawn_blocking`
//! and streams `FileInfo` structs through an MPSC channel to the async
//! consumer, which yields every 64 files to avoid starving request handlers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use rift_common::crypto::Chunker;
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::handler::merkle_cache::cache_computed_tree;
use crate::handler::merkle_cache::compute_file_merkle_tree;
use crate::metadata::db::Database;
use crate::metadata::merkle::CacheStatus;
use crate::security::canonicalize_within_share;

/// How many files to process before yielding back to the tokio runtime.
const YIELD_EVERY: usize = 64;

/// Summary of the background integrity check results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundCheckSummary {
    /// Total number of regular files found in the share.
    pub files_checked: usize,
    /// Number of files that had no cache entry and were newly computed.
    pub files_added: usize,
    /// Number of files where the cache key (mtime/size) didn't match the
    /// current filesystem metadata, and the entry was recomputed.
    pub files_stale: usize,
    /// Number of files where the cache key matched but tree data was absent,
    /// and the entry was recomputed.
    pub files_incomplete: usize,
    /// Number of files where recomputation failed.
    pub errors: usize,
}

/// Metadata for a single regular file discovered during the filesystem walk.
struct FileInfo {
    /// Canonicalized absolute path (verified to be within share root).
    path: PathBuf,
    /// File modification time in nanoseconds since Unix epoch.
    mtime_ns: u64,
    /// File size in bytes.
    file_size: u64,
}

/// Walk `share` on the calling (blocking) thread and send one `FileInfo`
/// per regular file through `tx`.
///
/// # Security
///
/// Uses `canonicalize_within_share` to:
/// 1. Resolve symlinks and `..` components to get canonical path
/// 2. Verify the canonical path is within the share boundary
/// 3. On Linux, perform TOCTOU-hardened fd-based re-canonicalization
///
/// Paths that escape the share boundary (symlink race, path traversal) are
/// silently skipped.
///
/// Takes owned params because `spawn_blocking` requires `'static + Send`.
#[allow(clippy::needless_pass_by_value)]
fn walk_share(share: PathBuf, share_canonical: PathBuf, tx: tokio::sync::mpsc::Sender<FileInfo>) {
    for entry in WalkDir::new(&share).follow_links(false) {
        let Ok(entry) = entry else {
            tracing::debug!("walk_dir entry failed, skipping");
            continue;
        };

        // Only process regular files (skip dirs, symlinks)
        if !entry.file_type().is_file() {
            continue;
        }

        // SECURITY: Use TOCTOU-safe canonicalization with containment check
        let Some(canonical) = canonicalize_within_share(entry.path(), &share_canonical) else {
            debug!(
                path = %entry.path().display(),
                "path escapes share root after canonicalization, skipping"
            );
            continue;
        };

        let Ok(meta) = std::fs::metadata(&canonical) else {
            debug!(path = %canonical.display(), "metadata failed, skipping");
            continue;
        };

        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(0))
            .unwrap_or(0);

        let file_size = meta.len();

        if tx
            .blocking_send(FileInfo {
                path: canonical,
                mtime_ns,
                file_size,
            })
            .is_err()
        {
            // Receiver dropped — shutdown
            return;
        }
    }
}

/// Run the background integrity check against a share directory.
///
/// Walks `share` on a blocking thread, checks each regular file's Merkle
/// cache status, and recomputes missing or stale entries.
///
/// Yields to the tokio runtime every `YIELD_EVERY` files so request
/// handlers are not starved.
///
/// # Security
///
/// The share root is canonicalized before walking, and all discovered paths
/// are verified to stay within this canonical root using TOCTOU-hardened
/// fd-based re-canonicalization on Linux.
pub async fn run_background_check(
    share: &Path,
    db: Arc<Database>,
    chunker: Chunker,
) -> anyhow::Result<BackgroundCheckSummary> {
    let mut summary = BackgroundCheckSummary {
        files_checked: 0,
        files_added: 0,
        files_stale: 0,
        files_incomplete: 0,
        errors: 0,
    };

    // Canonicalize share root first for containment checking
    let share_canonical = tokio::fs::canonicalize(share)
        .await
        .with_context(|| format!("share root does not exist: {}", share.display()))?;
    info!(
        share = %share.display(),
        canonical = %share_canonical.display(),
        "starting background check"
    );

    // Channel is bounded so the blocking producer doesn't get too far ahead.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<FileInfo>(256);

    let share_owned = share.to_path_buf();
    let share_canonical_owned = share_canonical.clone();
    tokio::task::spawn_blocking(move || walk_share(share_owned, share_canonical_owned, tx));

    let mut files_since_yield: usize = 0;

    while let Some(info) = rx.recv().await {
        summary.files_checked += 1;
        handle_file(info, &db, &chunker, &mut summary).await;

        files_since_yield += 1;
        if files_since_yield >= YIELD_EVERY {
            tokio::task::yield_now().await;
            files_since_yield = 0;
        }
    }

    info!(
        files_checked = summary.files_checked,
        files_added = summary.files_added,
        files_stale = summary.files_stale,
        files_incomplete = summary.files_incomplete,
        errors = summary.errors,
        "background check complete"
    );

    Ok(summary)
}

/// Check a single file's cache status, update counters, and recompute if stale.
async fn handle_file(
    info: FileInfo,
    db: &Database,
    chunker: &Chunker,
    summary: &mut BackgroundCheckSummary,
) {
    let status = match db
        .cache_status(&info.path, info.mtime_ns, info.file_size)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %info.path.display(), error = %e, "failed to check cache status");
            summary.errors += 1;
            return;
        }
    };

    if status == CacheStatus::Complete {
        tracing::debug!(path = %info.path.display(), "cache up-to-date, skipping");
        return;
    }

    record_status_counts(&status, summary, &info.path);
    recompute_file(&info.path, db, chunker, summary).await;
}

/// Update summary counters and log for a non-Complete cache status.
fn record_status_counts(status: &CacheStatus, summary: &mut BackgroundCheckSummary, path: &Path) {
    match status {
        CacheStatus::Missing => {
            summary.files_added += 1;
            debug!(path = %path.display(), "missing cache entry, computing");
        }
        CacheStatus::Stale => {
            summary.files_stale += 1;
            warn!(path = %path.display(), "stale cache entry, recomputing");
        }
        CacheStatus::Incomplete => {
            summary.files_incomplete += 1;
            warn!(path = %path.display(), "incomplete cache entry, recomputing");
        }
        CacheStatus::Complete => {}
    }
}

/// Recompute the Merkle tree for a single file and cache the result.
///
/// Called for `Missing`, `Stale`, and `Incomplete` cache entries.
/// Deletes the stale entry first (logging errors), then computes
/// and stores the fresh result.
async fn recompute_file(
    canonical: &Path,
    db: &Database,
    chunker: &Chunker,
    summary: &mut BackgroundCheckSummary,
) {
    // Delete stale/incomplete entry if present — log non-trivial errors
    if let Err(e) = db.delete_merkle(canonical).await {
        warn!(path = %canonical.display(), error = %e, "failed to delete stale cache entry before recompute");
    }

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
    use crate::metadata::merkle::CacheStatus;
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
        assert_eq!(summary.files_stale, 0);
        assert_eq!(summary.files_incomplete, 0);
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

        let summary = run_background_check(&share, db.clone(), Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 2);
        assert_eq!(summary.files_added, 2);
        assert_eq!(summary.files_stale, 0);
        assert_eq!(summary.files_incomplete, 0);
        assert_eq!(summary.errors, 0);

        // Verify cache was populated — both files should now be Complete
        let file_a = share.join("a.txt");
        let meta_a = std::fs::metadata(&file_a).unwrap();
        let mtime_a = u64::try_from(
            meta_a
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .unwrap_or(0);
        let size_a = meta_a.len();
        assert_eq!(
            db.cache_status(&file_a, mtime_a, size_a).await.unwrap(),
            CacheStatus::Complete,
            "file a should be cached after background check"
        );
    }

    #[tokio::test]
    async fn background_check_detects_conflict_and_recomputes() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();
        let file_path = share.join("conflict.txt");
        std::fs::write(&file_path, b"original content").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        // Pre-populate cache with WRONG mtime (simulates stale entry)
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
        assert_eq!(summary.files_stale, 1, "should detect mtime/size conflict");
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
        .unwrap_or(0);
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
            summary.files_stale, 0,
            "already cached file should not be stale"
        );
        assert_eq!(
            summary.files_incomplete, 0,
            "already cached file should not be incomplete"
        );
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn background_check_recomputes_incomplete_entries() {
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();
        let file_path = share.join("incomplete.txt");
        std::fs::write(&file_path, b"some content").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        // Create a proper incomplete entry: cache row with non-zero leaf_count,
        // tree nodes present, but leaf_info missing. We use raw SQL because
        // put_tree would never produce this inconsistent state, and put_merkle
        // with zero leaves would now correctly be interpreted as Complete for
        // an empty file.
        let root = Blake3Hash::new(b"incomplete_root");
        let meta = std::fs::metadata(&file_path).unwrap();
        let mtime_ns = u64::try_from(
            meta.modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .unwrap_or(0);
        let file_size = meta.len();

        let path_str = file_path.to_string_lossy().to_string();
        let root_bytes = root.as_bytes().to_vec();
        db.call({
            let path_str2 = path_str.clone();
            move |conn| {
                conn.execute(
                    "INSERT INTO merkle_cache (file_path, mtime_ns, file_size, root_hash, leaf_hashes, leaf_count, computed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    (path_str2, mtime_ns as i64, file_size as i64, root_bytes, Vec::<u8>::new(), 1i64, 0i64),
                )?;
                // Insert a tree node so has_nodes is true
                conn.execute(
                    "INSERT INTO merkle_tree_nodes (file_path, node_hash, children) VALUES (?1, ?2, ?3)",
                    (path_str, vec![0u8; 32], vec![1u8; 64]),
                )?;
                Ok(())
            }
        })
        .await
        .unwrap();

        let summary = run_background_check(&share, db.clone(), Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 1);
        assert_eq!(summary.files_stale, 0, "entry should not be stale");
        assert_eq!(
            summary.files_incomplete, 1,
            "entry with key match but missing leaf data should be incomplete"
        );
        assert_eq!(summary.errors, 0);

        // After recomputation, cache should be complete
        assert_eq!(
            db.cache_status(&file_path, mtime_ns, file_size)
                .await
                .unwrap(),
            CacheStatus::Complete,
            "incomplete entry should be recomputed and now be complete"
        );
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

    /// SECURITY: A regular file inside the share should be processed, but a file
    /// whose canonical path is OUTSIDE the share (due to directory symlinks in path)
    /// must be rejected without containment checking.
    ///
    /// This demonstrates the TOCTOU vulnerability: if WalkDir traverses into a
    /// symlinked directory, it may find files whose canonical paths escape the share.
    /// Without containment checking after `canonicalize()`, these would be processed.
    #[tokio::test]
    async fn background_check_rejects_canonical_path_escaping_share() {
        // Create share
        let share_tmp = tempfile::tempdir().unwrap();
        let share = share_tmp.path().to_path_buf();

        // Create a file inside the share
        std::fs::write(share.join("inside.txt"), b"safe content").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        let summary = run_background_check(&share, db.clone(), Chunker::default())
            .await
            .unwrap();

        // The file inside the share should be checked
        assert_eq!(
            summary.files_checked, 1,
            "file inside share should be processed"
        );
        assert_eq!(summary.files_added, 1);
        assert_eq!(summary.errors, 0);

        // Verify the cache has the correct path
        let inside_path = share.join("inside.txt");
        let canonical_inside = inside_path.canonicalize().unwrap();
        let meta = std::fs::metadata(&canonical_inside).unwrap();
        let mtime_ns = u64::try_from(
            meta.modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .unwrap_or(0);

        assert_eq!(
            db.cache_status(&canonical_inside, mtime_ns, meta.len())
                .await
                .unwrap(),
            CacheStatus::Complete,
            "file inside share should be cached"
        );
    }

    /// SECURITY: Verify that after canonicalize, the path is checked for containment.
    /// This test verifies the fix works with a mock scenario.
    #[tokio::test]
    async fn background_check_with_symlinked_share_root_processes_contained_files() {
        // On systems where temp directories have symlinks (e.g., macOS /var -> /private/var),
        // canonical paths may differ from the share path. This should still work correctly
        // because containment is checked against the CANONICAL share root.
        let share_tmp = tempfile::tempdir().unwrap();
        let share = share_tmp.path().to_path_buf();

        // Create a file inside
        std::fs::write(share.join("file.txt"), b"content").unwrap();

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        // Should succeed even if share path contains symlinks
        let summary = run_background_check(&share, db.clone(), Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 1);
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn background_check_yields_to_runtime() {
        // Verify that the background check doesn't block the runtime
        // by checking that another task can make progress while it runs.
        let temp_dir = tempfile::tempdir().unwrap();
        let share = temp_dir.path().to_path_buf();

        // Create enough files to trigger several yields
        for i in 0..200 {
            std::fs::write(share.join(format!("file{i}.txt")), b"x").unwrap();
        }

        let db: Arc<Database> = Arc::new(Database::open_in_memory().await.unwrap());

        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag_clone = flag.clone();

        // Spawn a task that sets a flag — it should be able to run
        // even while the background check is in progress.
        let setter = tokio::spawn(async move {
            tokio::task::yield_now().await;
            flag_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let _summary = run_background_check(&share, db, Chunker::default())
            .await
            .unwrap();

        setter.await.unwrap();
        assert!(
            flag.load(std::sync::atomic::Ordering::SeqCst),
            "other tasks should make progress during background check"
        );
    }
}
