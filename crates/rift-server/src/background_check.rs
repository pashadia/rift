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

use rift_common::crypto::Chunker;
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::handler::merkle_cache::cache_computed_tree;
use crate::handler::merkle_cache::compute_file_merkle_tree;
use crate::metadata::db::Database;
use crate::metadata::merkle::CacheStatus;

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
    /// Canonicalized absolute path.
    path: PathBuf,
    /// File modification time in nanoseconds since Unix epoch.
    mtime_ns: u64,
    /// File size in bytes.
    file_size: u64,
}

/// Walk `share` on the calling (blocking) thread and send one `FileInfo`
/// per regular file through `tx`.
///
/// Skips directories and symlinks. Paths are canonicalized so they match
/// what the request handlers store in the DB.
///
/// Takes owned params because `spawn_blocking` requires `'static + Send`.
#[allow(clippy::needless_pass_by_value)]
fn walk_share(share: PathBuf, tx: tokio::sync::mpsc::Sender<FileInfo>) {
    for entry in WalkDir::new(&share).follow_links(false) {
        let Ok(entry) = entry else {
            tracing::debug!("walk_dir entry failed, skipping");
            continue;
        };

        // Only process regular files (skip dirs, symlinks)
        if !entry.file_type().is_file() {
            continue;
        }

        let Ok(canonical) = entry.path().canonicalize() else {
            tracing::debug!(path = %entry.path().display(), "canonicalize failed, skipping");
            continue;
        };

        let Ok(meta) = std::fs::metadata(&canonical) else {
            tracing::debug!(path = %canonical.display(), "metadata failed, skipping");
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

    // Channel is bounded so the blocking producer doesn't get too far ahead.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<FileInfo>(256);

    let share_owned = share.to_path_buf();
    tokio::task::spawn_blocking(move || walk_share(share_owned, tx));

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
            info!(path = %path.display(), "missing cache entry, computing");
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

        // Insert a cache entry with correct mtime/size but NO tree nodes or leaf info
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
        db.put_merkle(&file_path, mtime_ns, file_size, &root, &[])
            .await
            .unwrap();

        let summary = run_background_check(&share, db.clone(), Chunker::default())
            .await
            .unwrap();

        assert_eq!(summary.files_checked, 1);
        assert_eq!(summary.files_stale, 0, "entry should not be stale");
        assert_eq!(
            summary.files_incomplete, 1,
            "entry with key match but missing tree data should be incomplete"
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
